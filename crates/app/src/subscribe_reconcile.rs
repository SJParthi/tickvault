//! Expected-vs-first-seen subscription reconcile, with an in-place resend
//! (plan item 3b, `active-plan-feed-hardening.md`; owner 2026-10-30: "fix
//! resolve implement everything now").
//!
//! ## The failure this catches
//!
//! A subscribe message that Dhan silently does not take leaves no error, no
//! close code and no bad payload: the socket stays up, every other instrument
//! keeps ticking, and the instruments in the lost message simply never send a
//! packet (scenario #62, partial subscribe). The silence detector already
//! sees each such instrument as `NeverTicked`; nothing asked which SOCKET held
//! it or tried to put it back.
//!
//! ## What this does
//!
//! - **Expected set.** Every main-feed subscribe the lane sends is recorded
//!   here with the socket's command sender ([`SubscribeReconcile::record_subscribed`]),
//!   keyed on the I-P1-11 composite `(security_id, segment)`.
//! - **Delta.** On the drain's existing 30 s silence sweep (no new pass, no
//!   per-tick work) every `NeverTicked` report for an expected instrument that
//!   was recorded at least [`RECONCILE_DELTA_SECS`] ago counts as missing
//!   (`tv_subscribe_reconcile_missing`).
//! - **Resend, in place.** Inside the continuous session, on a socket that IS
//!   delivering for other instruments, the missing instruments are offered
//!   again through the existing `LiveSubscriptionCommand::Resubscribe` on the
//!   SAME socket: first an unsubscribe of them, then a subscribe of them. Never
//!   a close, never a redial for this (scope lock 2026-10-02). One round in
//!   flight per socket, at most [`RECONCILE_MAX_ROUNDS`] rounds per instrument
//!   per IST day, the next round no sooner than [`RECONCILE_ROUND_INTERVAL_SECS`]
//!   after the previous one FINISHED; then the instrument is counted as
//!   exhausted and named in one log line.
//! - **Switch.** `[dhan_universe] subscribe_reconcile_auto_resubscribe`,
//!   default `false`: the delta and the round planning run and are counted as
//!   `would_resubscribe`, nothing is sent, and no round is spent (so nothing
//!   is ever reported as exhausted or recovered in that mode).
//!
//! ## Why two commands and not one
//!
//! `Resubscribe` treats an instrument named on BOTH lists as "stays held, costs
//! nothing on the wire", so one command cannot re-send a held instrument. The
//! first command removes them from the socket and from the reconnect replay
//! list; the second adds back exactly the ones that left. The socket refuses a
//! whole unsubscribe if any one instrument in it is not held, so a refused
//! batch is split in halves until each refused part is a single instrument;
//! a not-held instrument is not on the socket at all, so it goes straight to
//! the subscribe half. The subscribe half is retried on a definite failure,
//! because between the two commands the instruments are off the socket.
//!
//! ## Gates (each one keeps a whole-socket or whole-market silence out of it)
//!
//! - Continuous session only, from 120 s after its open (09:17) to the
//!   confirmed-silence close, trading days, the same clock the frame-silence
//!   redials read.
//! - The latest sweep must observe the whole feed delivering: at least one expected instrument
//!   reads `Live`, or `Warming` with its quiet stretch inside the expected gap
//!   (`Warming` alone never looks at how long it has been quiet). An
//!   instrument that ticked earlier and has gone quiet (`Exceeded`, or a
//!   stale `Warming`) does not count, so a halt is not a partial subscribe.
//! - The latest sweep must observe the socket delivering for at least one other expected
//!   instrument; a socket where nothing is live is the frame-silence redial's
//!   job, counted here as `skipped_silent_socket` once per episode.
//! - Before each unsubscribe or known-not-held placement enqueue (including
//!   retries), recheck the current owner, reservation, IST day and session gate
//!   after channel capacity is available. Already removed or uncertain members
//!   are restored even if eligibility changes. The later socket write is not
//!   covered by this enqueue-time check.
//!
//! ## Complexity
//!
//! The drain calls `note_at_slot` (2026-11-17): while no round is reserved
//! today, a report whose detector slot's cell holds its key is judged from
//! that dense cell, O(1) with no hash; otherwise it falls back to `note`.
//! `note` is expected O(1) per report: one `ahash` probe into a map of at most
//! [`RECONCILE_MAX_EXPECTED`] entries, and no allocation: the per-socket
//! candidate lists are pre-sized to the per-round cap and never pushed past
//! it, and the exhausted instruments are a count plus the first
//! `NAMED_IN_LOG` in a list pre-sized once. It runs inside the drain's existing 30 s O(n) silence scan, so it
//! adds O(n) probes to that scan, never per tick: measured by
//! `sweep_cost_at_the_authorized_ceiling` (`#[ignore]`d, run it for the
//! number). `finish` is O(sockets + candidates), except once per IST day,
//! when the rollover walks every expected entry, O(expected).
//! `record_subscribed` is O(instruments) once per subscribe command, cold.
//! The sweep holds the book's `std::sync::Mutex` for the whole scan; the
//! other takers are cold (a subscribe record, the executor finishing a round),
//! and each holds it for O(its instruments). A cold taker that arrives during
//! a sweep waits for the rest of it, blocking its own tokio worker for up to
//! one sweep (the harness above measures that). No await happens under the
//! lock. Also under it, all rare: the edge-latched skip logs, the exhausted
//! log line and counter (at most once per instrument per day), and the
//! non-blocking `try_send` that hands a round to the executor (at most one
//! per socket per 120 s). Space is bounded: at most
//! [`RECONCILE_MAX_EXPECTED`] instruments (past it a new one is refused and
//! counted) and [`RECONCILE_MAX_SOCKETS`] sockets. The executor runs on its
//! own task: O(instruments) per round plus at most
//! [`RECONCILE_MAX_UNSUBSCRIBE_COMMANDS`] split commands.
//!
//! ## Honest limits
//!
//! - An instrument the silence detector does not track gives no report and is
//!   never judged. An instrument that produced even one packet is never judged
//!   again, so a subscribe lost on a later redial is not caught here.
//! - A contract that genuinely never trades can read as missing; with the
//!   switch on it costs at most three resends a day and then one log line
//!   naming it.
//! - Between the two commands the instruments are off the socket (one
//!   round-trip); a process stop in that gap drops them from the reconnect
//!   replay until the next attach.
//! - An unsubscribe whose answer never came back is treated as having
//!   happened: its instruments are subscribed again once. If it had in fact
//!   not happened, the socket's duplicate check drops them and logs its own
//!   WS-GAP-02 line.
//! - Putting back what a round took off is one all-or-nothing command (the
//!   socket checks its cap on the whole change). If another change on the
//!   same socket takes even one of the freed slots in between, the socket
//!   refuses the whole command, and after three attempts every one of them is
//!   reported lost (coded `error!`), not only the one that no longer fits.
//!   They return at the next attach or redial.
//! - The lane must call [`SubscribeReconcile::forget`] for any instrument it
//!   deliberately removes from a socket, or a round would subscribe it again.
//! - Local `/metrics` only: no alarm, no page, no CloudWatch metric.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use metrics::{counter, gauge};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};

use tickvault_common::constants::{IST_UTC_OFFSET_SECONDS_I64, SECONDS_PER_DAY};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::types::ExchangeSegment;
use tickvault_core::pipeline::tick_gap_detector::{SilenceReport, SilenceVerdict};
use tickvault_core::websocket::pool_supervisor::{
    CONTINUOUS_SESSION_OPEN_SECS_OF_DAY_IST, FrameSilenceGate, LiveSubscriptionCommand,
    ResubscribeOutcome, SubscribeInstrument,
};
use tickvault_trading::candles::multi_tf_aggregator::AGGREGATOR_MAX_SLOTS;

/// An expected instrument counts as missing once it was subscribed at least
/// this long ago and has still produced no packet (plan item 3b: "at T+120s").
pub const RECONCILE_DELTA_SECS: u64 = 120;

/// Resend rounds per instrument per IST day before it is reported exhausted
/// (plan item 3b: "max 3 rounds").
pub const RECONCILE_MAX_ROUNDS: u8 = 3;

/// Minimum spacing between the end of one round on a socket and the start of
/// the next. Matches the delta, so a resent instrument gets the same 120 s to
/// start before it is judged again.
pub const RECONCILE_ROUND_INTERVAL_SECS: u64 = 120;

/// Most instruments one round resends on one socket (ten 100-instrument
/// messages each way). A larger delta is taken over several rounds.
pub const RECONCILE_MAX_PER_SOCKET_PER_ROUND: usize = 1_000;

/// Most unsubscribe commands one round sends while splitting a refused batch.
/// Finding k not-held instruments among n costs about 2·k·log2(n) commands;
/// past this cap the rest of the round is left untouched and its rounds are
/// refunded.
pub const RECONCILE_MAX_UNSUBSCRIBE_COMMANDS: usize = 64;

/// Most expected instruments held: the daily universe ceiling (5 main-feed
/// sockets × 5,000), tied to it so a universe raise moves this too.
pub const RECONCILE_MAX_EXPECTED: usize = tickvault_common::constants::MAX_DAILY_UNIVERSE_SIZE;

/// Most sockets tracked (the 16-connection authorization, rounded up).
pub const RECONCILE_MAX_SOCKETS: usize = 32;

/// Rounds queued for the executor before a new one is refused.
pub const RECONCILE_EXECUTOR_QUEUE: usize = 16;

/// How long the executor waits to hand a command to a socket's task.
pub const RECONCILE_SEND_TIMEOUT_SECS: u64 = 30;

/// How long the executor waits for a socket's answer to one command.
pub const RECONCILE_ACK_TIMEOUT_SECS: u64 = 30;

/// Attempts at the subscribe half before the instruments are reported lost.
pub const RECONCILE_SUBSCRIBE_ATTEMPTS: u32 = 3;

/// Pause between two subscribe attempts.
pub const RECONCILE_SUBSCRIBE_RETRY_SECS: u64 = 2;

/// Instruments named in one exhausted, lost or unknown log line.
const NAMED_IN_LOG: usize = 20;

/// Slot cells for the silence sweep's fast path (2026-11-17): one per slot of
/// the drain's silence detector, so the 30 s sweep reads an expected
/// instrument's socket and record time by slot index instead of one hash probe
/// per report. Equal to the detector capacity the live lane builds
/// (`with_detector_capacity(AGGREGATOR_MAX_SLOTS)`), pinned by
/// `test_cell_capacity_equals_detector_capacity`. A slot past it is judged by
/// the probe, so a larger detector would only lose speed, never a verdict.
/// SPACE: 25,000 × 32 B = 800 KB, allocated once per book.
pub const RECONCILE_CELL_SLOTS: usize = AGGREGATOR_MAX_SLOTS;

/// `SlotCell::socket` of a cell that holds nothing (socket indices stay below
/// [`RECONCILE_MAX_SOCKETS`]).
const CELL_EMPTY: u8 = u8::MAX;

/// `Expected::view_slot` of an instrument no cell holds.
const VIEW_SLOT_NONE: u32 = u32::MAX;

/// Counter: one per round decision or executor result, by `outcome`.
pub const RECONCILE_ROUNDS_METRIC: &str = "tv_subscribe_reconcile_rounds_total";

/// Counter: instruments by what happened to them, by `outcome`.
pub const RECONCILE_INSTRUMENTS_METRIC: &str = "tv_subscribe_reconcile_instruments_total";

/// Gauge: expected instruments missing at the last sweep.
pub const RECONCILE_MISSING_GAUGE: &str = "tv_subscribe_reconcile_missing";

/// Every `outcome` label of [`RECONCILE_ROUNDS_METRIC`], registered at 0.
const ROUND_OUTCOMES: [&str; 16] = [
    "would_resubscribe",
    "dispatched",
    "queue_full",
    "skipped_silent_socket",
    "skipped_feed_silent",
    "resubscribed",
    "refused_halted_805",
    "send_failed",
    "unsubscribe_split",
    "unsubscribe_unknown",
    "subscribe_retried",
    "subscribe_unknown",
    "split_cap_reached",
    "lost",
    "canceled_stale",
    "round_id_exhausted",
];

/// Every `outcome` label of [`RECONCILE_INSTRUMENTS_METRIC`], registered at 0.
const INSTRUMENT_OUTCOMES: [&str; 9] = [
    "unplaceable",
    "missing_found",
    "resent",
    "not_held",
    "recovered",
    "exhausted",
    "unknown",
    "lost",
    "refused_untracked",
];

type Key = (u64, ExchangeSegment);

/// The expected set. `ahash` rather than the default SipHash (2026-11-17): the
/// cold-path probes (record, forget, a round's reservations) and the sweep's
/// slow path are still one probe each, about a third the cost; no iteration
/// order is ever read except the once-a-day reset, which visits every entry.
type ExpectedMap = HashMap<Key, Expected, ahash::RandomState>;

fn key_of(instrument: &SubscribeInstrument) -> Key {
    (instrument.security_id, instrument.segment)
}

/// One expected instrument.
#[derive(Debug, Clone, Copy)]
struct Expected {
    instrument: SubscribeInstrument,
    socket: u8,
    recorded_at: Instant,
    /// Daily reserved or consumed rounds (shadow mode never spends one).
    rounds: u8,
    /// Unique reservation still queued/executing for this current ownership.
    reserved_round: Option<u64>,
    /// Counted on `missing_found` today.
    missing_counted: bool,
    exhausted_reported: bool,
    recovered_reported: bool,
    /// The slot whose cell holds this instrument, or [`VIEW_SLOT_NONE`].
    /// Invariant: when set, `cells[view_slot]` is valid and holds this key,
    /// this socket and this record time.
    view_slot: u32,
}

/// One silence-detector slot's copy of the fields the sweep's fast path reads
/// (2026-11-17). Valid while `socket != CELL_EMPTY`; it is emptied the moment
/// the instrument it holds is forgotten or rebound to another socket, so a
/// valid cell always equals its expected entry's `socket` and `recorded_at`.
#[derive(Debug, Clone, Copy)]
struct SlotCell {
    security_id: u64,
    recorded_at: Instant,
    segment: ExchangeSegment,
    socket: u8,
}

/// One socket the lane subscribed on.
#[derive(Debug)]
struct Socket {
    tx: mpsc::Sender<LiveSubscriptionCommand>,
    /// When the last round on this socket finished (shadow: when it was
    /// planned).
    last_round_at: Option<Instant>,
    /// A round for this socket is queued or running.
    in_flight: Option<u64>,
    /// Expected instruments delivering now (`Live`, or a recent `Warming`) this
    /// sweep.
    delivering: u32,
    candidates: Vec<SubscribeInstrument>,
    silent_reported: bool,
}

#[derive(Debug)]
struct Inner {
    sockets: Vec<Socket>,
    expected: ExpectedMap,
    /// [`RECONCILE_CELL_SLOTS`] cells, indexed by silence-detector slot,
    /// filled once at construction and never grown.
    cells: Vec<SlotCell>,
    /// Set the first time a round is reserved today, cleared only by the day
    /// reset. While it is clear every entry has `rounds == 0`, which is what
    /// lets the fast path skip `mark_recovered` and the exhausted branch.
    rounds_any: bool,
    /// Reports judged on the fast path since the book was built, added at
    /// each `finish_at` (2026-11-17 review: the proof the fast path runs,
    /// read by the DHAT gate). Never reset.
    fast_path_hits: u64,
    /// The first [`NAMED_IN_LOG`] instruments that used their last round in
    /// the current sweep, for the log line. Pre-sized once and cleared per
    /// sweep, so `note` never allocates; the full count is kept apart.
    exhausted_named: Vec<SubscribeInstrument>,
    ist_day: i64,
    feed_silent_reported: bool,
    next_reservation: u64,
}

/// One planned resend, handed to the executor.
#[derive(Debug)]
pub struct PlannedRound {
    tx: mpsc::Sender<LiveSubscriptionCommand>,
    socket: u8,
    instruments: Vec<SubscribeInstrument>,
    reservation: u64,
    ist_day: i64,
}

impl PlannedRound {
    /// Test-only: the round's socket, instruments, reservation and IST day.
    #[cfg(test)]
    pub(crate) fn test_view(&self) -> (u8, Vec<SubscribeInstrument>, u64, i64) {
        (
            self.socket,
            self.instruments.clone(),
            self.reservation,
            self.ist_day,
        )
    }
}

/// What one sweep found and did. Returned for the caller's tests and logs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepSummary {
    /// Expected instruments past the delta with no packet yet.
    pub missing: u32,
    /// Rounds handed to the executor this sweep.
    pub dispatched: u32,
    /// Rounds that would have been handed over with the switch on.
    pub would_resubscribe: u32,
    /// Instruments that reached the round limit this sweep.
    pub newly_exhausted: u32,
}

/// The expected-set book plus the executor queue. Shared by the attach paths
/// (which record) and the drain's silence sweep (which judges).
#[derive(Debug)]
pub struct SubscribeReconcile {
    inner: Mutex<Inner>,
    auto_resubscribe: bool,
    rounds_tx: mpsc::Sender<PlannedRound>,
}

impl SubscribeReconcile {
    /// Builds the book and the executor's receiving end. `auto_resubscribe`
    /// is `[dhan_universe] subscribe_reconcile_auto_resubscribe`.
    #[must_use]
    pub fn new(auto_resubscribe: bool) -> (Arc<Self>, mpsc::Receiver<PlannedRound>) {
        for outcome in ROUND_OUTCOMES {
            counter!(RECONCILE_ROUNDS_METRIC, "outcome" => outcome).increment(0);
        }
        for outcome in INSTRUMENT_OUTCOMES {
            counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => outcome).increment(0);
        }
        gauge!(RECONCILE_MISSING_GAUGE).set(0.0);
        let (rounds_tx, rounds_rx) = mpsc::channel(RECONCILE_EXECUTOR_QUEUE);
        let book = Self {
            inner: Mutex::new(Inner {
                sockets: Vec::with_capacity(RECONCILE_MAX_SOCKETS),
                expected: HashMap::with_capacity_and_hasher(
                    RECONCILE_MAX_EXPECTED,
                    ahash::RandomState::default(),
                ),
                cells: vec![
                    SlotCell {
                        security_id: 0,
                        recorded_at: Instant::now(),
                        segment: ExchangeSegment::IdxI,
                        socket: CELL_EMPTY,
                    };
                    RECONCILE_CELL_SLOTS
                ],
                rounds_any: false,
                fast_path_hits: 0,
                exhausted_named: Vec::with_capacity(NAMED_IN_LOG),
                ist_day: ist_day_now(),
                feed_silent_reported: false,
                next_reservation: 0,
            }),
            auto_resubscribe,
            rounds_tx,
        };
        (Arc::new(book), rounds_rx)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A poisoned lock means a panic elsewhere already took the process
        // down under `panic = "abort"`; in tests, keep reading the state.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records instruments the lane just asked `tx`'s main-feed socket to
    /// subscribe. Call it beside every boot subscribe and every `Extend` on a
    /// main-feed socket. An instrument already recorded keeps its first live
    /// socket (a backup copy is not the copy being reconciled). A closed primary
    /// can be replaced, preserving the instrument's spent daily rounds.
    ///
    /// Cold path: O(instruments) once per subscribe command.
    pub fn record_subscribed(
        &self,
        tx: &mpsc::Sender<LiveSubscriptionCommand>,
        instruments: &[SubscribeInstrument],
    ) {
        self.record_subscribed_at(tx, instruments, Instant::now());
    }

    pub(crate) fn record_subscribed_at(
        &self,
        tx: &mpsc::Sender<LiveSubscriptionCommand>,
        instruments: &[SubscribeInstrument],
        now: Instant,
    ) {
        let mut inner = self.lock();
        let socket = match inner.sockets.iter().position(|s| s.tx.same_channel(tx)) {
            Some(index) => index,
            None if inner.sockets.len() < RECONCILE_MAX_SOCKETS => {
                inner.sockets.push(Socket {
                    tx: tx.clone(),
                    last_round_at: None,
                    in_flight: None,
                    delivering: 0,
                    // Pre-sized to the per-round cap, so `note` never grows
                    // it on the drain.
                    candidates: Vec::with_capacity(RECONCILE_MAX_PER_SOCKET_PER_ROUND),
                    silent_reported: false,
                });
                inner.sockets.len() - 1
            }
            None => {
                counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "refused_untracked")
                    .increment(instruments.len() as u64);
                return;
            }
        };
        let socket = u8::try_from(socket).unwrap_or(u8::MAX);
        let mut refused: u64 = 0;
        for instrument in instruments {
            let key = key_of(instrument);
            if let Some(entry) = inner.expected.get(&key) {
                let previous_closed = inner
                    .sockets
                    .get(usize::from(entry.socket))
                    .is_some_and(|slot| slot.tx.is_closed());
                let Inner {
                    expected, cells, ..
                } = &mut *inner;
                if previous_closed && let Some(entry) = expected.get_mut(&key) {
                    entry.socket = socket;
                    entry.recorded_at = now;
                    entry.reserved_round = None;
                    // The cached socket and record time are stale now.
                    release_cell(cells, entry);
                }
                continue;
            }
            if inner.expected.len() >= RECONCILE_MAX_EXPECTED {
                refused = refused.saturating_add(1);
                continue;
            }
            inner.expected.insert(
                key,
                Expected {
                    instrument: *instrument,
                    socket,
                    recorded_at: now,
                    rounds: 0,
                    reserved_round: None,
                    missing_counted: false,
                    exhausted_reported: false,
                    recovered_reported: false,
                    view_slot: VIEW_SLOT_NONE,
                },
            );
        }
        if refused > 0 {
            counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "refused_untracked")
                .increment(refused);
        }
    }

    /// Stops tracking instruments the lane deliberately removed from a socket.
    pub fn forget(&self, instruments: &[SubscribeInstrument]) {
        let mut inner = self.lock();
        let Inner {
            expected, cells, ..
        } = &mut *inner;
        for instrument in instruments {
            if let Some(mut removed) = expected.remove(&key_of(instrument)) {
                release_cell(cells, &mut removed);
            }
        }
    }

    /// Removes a refused/truncated offer only while this socket owns it.
    /// A backup refusal or a delayed ACK from a closed former primary cannot
    /// erase another socket's expected subscription.
    pub(crate) fn forget_on(
        &self,
        tx: &mpsc::Sender<LiveSubscriptionCommand>,
        instruments: &[SubscribeInstrument],
    ) {
        let mut inner = self.lock();
        for instrument in instruments {
            let key = key_of(instrument);
            let owned = inner.expected.get(&key).is_some_and(|entry| {
                inner
                    .sockets
                    .get(usize::from(entry.socket))
                    .is_some_and(|slot| slot.tx.same_channel(tx))
            });
            if owned {
                let Inner {
                    expected, cells, ..
                } = &mut *inner;
                if let Some(mut removed) = expected.remove(&key) {
                    release_cell(cells, &mut removed);
                }
            }
        }
    }

    /// Instruments currently expected.
    #[must_use]
    pub fn expected_len(&self) -> usize {
        self.lock().expected.len()
    }

    /// Slot cells currently valid (the sweep's fast path reads them).
    /// Diagnostic: O([`RECONCILE_CELL_SLOTS`]) under the book's lock, never
    /// on the drain.
    #[must_use]
    pub fn cached_cells(&self) -> usize {
        self.lock()
            .cells
            .iter()
            .filter(|cell| cell.socket != CELL_EMPTY)
            .count()
    }

    /// Reports judged on [`ReconcileSweep::note_at_slot`]'s fast path (the
    /// slot cell, no hash) since the book was built, counted at each
    /// `finish_at`. Diagnostic: O(1) under the book's lock, never on the drain.
    #[must_use]
    pub fn fast_path_hits(&self) -> u64 {
        self.lock().fast_path_hits
    }

    /// Test-only: everything a sweep can change, as text, minus the slot
    /// cells and `view_slot` (a book judged only through `note` keeps none).
    /// Two books that saw the same records and reports print the same text.
    #[cfg(test)]
    pub(crate) fn test_fingerprint(&self) -> String {
        use std::fmt::Write as _;
        let inner = self.lock();
        let mut entries: Vec<_> = inner.expected.iter().collect();
        entries.sort_by_key(|(k, _)| (k.0, k.1.as_str()));
        let mut out = String::new();
        for (k, e) in entries {
            let _ = writeln!(
                out,
                "{k:?} s={} at={:?} r={} res={:?} m={} x={} rec={}",
                e.socket,
                e.recorded_at,
                e.rounds,
                e.reserved_round,
                e.missing_counted,
                e.exhausted_reported,
                e.recovered_reported
            );
        }
        for (i, s) in inner.sockets.iter().enumerate() {
            let candidates: Vec<Key> = s.candidates.iter().map(key_of).collect();
            let _ = writeln!(
                out,
                "socket {i} last={:?} flight={:?} d={} c={candidates:?} silent={}",
                s.last_round_at, s.in_flight, s.delivering, s.silent_reported
            );
        }
        let named: Vec<Key> = inner.exhausted_named.iter().map(key_of).collect();
        let _ = writeln!(
            out,
            "named={named:?} day={} feed_silent={} next={}",
            inner.ist_day, inner.feed_silent_reported, inner.next_reservation
        );
        out
    }

    /// Test-only: checks the cell invariant both ways. Every valid cell at
    /// slot `s` holding key `K` has `expected[K].view_slot == s` with the same
    /// socket and record time, and every entry with a slot is held by that
    /// slot's valid cell.
    #[cfg(test)]
    pub(crate) fn assert_cell_invariant(&self) {
        let inner = self.lock();
        for (slot, cell) in inner.cells.iter().enumerate() {
            if cell.socket == CELL_EMPTY {
                continue;
            }
            // An assert, not a panic macro: `production_panic_macro_guard` counts
            // those above `mod tests`, and this `#[cfg(test)]` fn sits above it.
            let entry = inner.expected.get(&(cell.security_id, cell.segment));
            assert!(
                entry.is_some(),
                "valid cell {slot} holds a key not expected"
            );
            let Some(entry) = entry else { continue };
            assert_eq!(entry.view_slot as usize, slot, "cell {slot} view_slot");
            assert_eq!(entry.socket, cell.socket, "cell {slot} socket");
            assert_eq!(
                entry.recorded_at, cell.recorded_at,
                "cell {slot} recorded_at"
            );
        }
        for (key, entry) in &inner.expected {
            if entry.view_slot == VIEW_SLOT_NONE {
                continue;
            }
            let cell = inner.cells[entry.view_slot as usize];
            assert!(cell.socket != CELL_EMPTY, "{key:?} points at an empty cell");
            assert_eq!((cell.security_id, cell.segment), *key, "{key:?} cell key");
        }
    }

    /// Starts one judging pass. Feed it every report of the drain's silence
    /// sweep with [`ReconcileSweep::note`], then call
    /// [`ReconcileSweep::finish_now`]. Holds the book's lock until finished.
    #[must_use]
    pub fn sweep(&self) -> ReconcileSweep<'_> {
        self.sweep_at(Instant::now())
    }

    pub(crate) fn sweep_at(&self, now: Instant) -> ReconcileSweep<'_> {
        let mut inner = self.lock();
        for socket in &mut inner.sockets {
            socket.delivering = 0;
            socket.candidates.clear();
        }
        inner.exhausted_named.clear();
        ReconcileSweep {
            book: self,
            inner,
            now,
            missing: 0,
            exhausted: 0,
            fast_hits: 0,
        }
    }

    /// The executor's report that this reserved round is over: its socket may
    /// plan again [`RECONCILE_ROUND_INTERVAL_SECS`] from now, and the
    /// instruments in `refund` get their matching current-day reservation back.
    fn round_finished(&self, round: &PlannedRound, refund: &[SubscribeInstrument], now: Instant) {
        let mut inner = self.lock();
        let same_day = inner.ist_day == round.ist_day;
        let Inner {
            sockets, expected, ..
        } = &mut *inner;
        if same_day {
            refund_reservation(expected, refund, round.reservation);
            for instrument in &round.instruments {
                if let Some(entry) = expected.get_mut(&key_of(instrument))
                    && entry.reserved_round == Some(round.reservation)
                {
                    entry.reserved_round = None;
                }
            }
        }
        if let Some(slot) = sockets.get_mut(usize::from(round.socket))
            && slot.in_flight == Some(round.reservation)
        {
            slot.in_flight = None;
            slot.last_round_at = Some(now);
        }
    }

    /// Called after obtaining command-channel capacity, immediately before
    /// enqueue. A stale member cancels new work; restoration of removed or
    /// unknown members deliberately bypasses the eligibility gate. This does
    /// not revalidate at the later socket write, and delivering/first-packet
    /// evidence remains the latest silence sweep's observation.
    fn send_round_change<C>(
        &self,
        round: &PlannedRound,
        instruments: &[SubscribeInstrument],
        kind: RoundCommand,
        permit: mpsc::Permit<'_, LiveSubscriptionCommand>,
        ack: oneshot::Sender<ResubscribeOutcome>,
        clock: &mut C,
    ) -> bool
    where
        C: FnMut() -> (u32, i64),
    {
        if matches!(kind, RoundCommand::Restore) {
            permit.send(kind.command(instruments, ack));
            return true;
        }
        if !self.auto_resubscribe {
            return false;
        }
        let inner = self.lock();
        let (secs_of_day_ist, ist_day) = clock();
        if !reconcile_window_open(secs_of_day_ist)
            || inner.ist_day != round.ist_day
            || ist_day != round.ist_day
        {
            return false;
        }
        let Some(socket) = inner.sockets.get(usize::from(round.socket)) else {
            return false;
        };
        let current = socket.in_flight == Some(round.reservation)
            && socket.tx.same_channel(&round.tx)
            && !socket.tx.is_closed()
            && socket.delivering > 0
            && instruments.iter().all(|instrument| {
                inner
                    .expected
                    .get(&key_of(instrument))
                    .is_some_and(|entry| {
                        entry.socket == round.socket
                            && entry.reserved_round == Some(round.reservation)
                            && !entry.recovered_reported
                    })
            });
        if current {
            // Capacity was acquired before this lock. Keep the decision and
            // synchronous enqueue together so forget/rebind cannot interleave.
            permit.send(kind.command(instruments, ack));
        }
        current
    }
}

fn reconcile_window_open(secs_of_day_ist: u32) -> bool {
    FrameSilenceGate::ContinuousSessionIst.confirmed_window_open_at(secs_of_day_ist)
        && u64::from(secs_of_day_ist)
            >= u64::from(CONTINUOUS_SESSION_OPEN_SECS_OF_DAY_IST)
                .saturating_add(RECONCILE_DELTA_SECS)
}

/// One judging pass over the silence reports. See [`SubscribeReconcile::sweep`].
#[derive(Debug)]
pub struct ReconcileSweep<'a> {
    book: &'a SubscribeReconcile,
    inner: MutexGuard<'a, Inner>,
    now: Instant,
    missing: u32,
    /// Instruments that used their last round in this sweep (the first
    /// [`NAMED_IN_LOG`] are named in `Inner::exhausted_named`).
    exhausted: u32,
    /// Reports this sweep judged on [`Self::note_at_slot`]'s fast path.
    fast_hits: u32,
}

impl ReconcileSweep<'_> {
    /// Judges one silence report: expected O(1), one hash probe. Returns
    /// whether the book expects the report's key (`false` only on a miss), so
    /// [`Self::note_at_slot`] skips its second probe for a key it never holds.
    pub fn note(&mut self, report: &SilenceReport) -> bool {
        let now = self.now;
        let inner = &mut *self.inner;
        let Some(entry) = inner.expected.get_mut(&report.key) else {
            return false;
        };
        let Some(socket) = inner.sockets.get_mut(usize::from(entry.socket)) else {
            return true;
        };
        match report.verdict {
            SilenceVerdict::NeverTicked => {}
            SilenceVerdict::Live => {
                socket.delivering = socket.delivering.saturating_add(1);
                mark_recovered(entry);
                return true;
            }
            // Ticked, but too few gaps for a baseline. `Warming` never looks at
            // how long it has been quiet, so a thin contract that ticked at
            // 09:05 still reads `Warming` hours later: it counts as delivering
            // only while its quiet stretch is inside the expected gap.
            SilenceVerdict::Warming => {
                if report.silent_millis <= report.expected_millis {
                    socket.delivering = socket.delivering.saturating_add(1);
                }
                mark_recovered(entry);
                return true;
            }
            // Ticked earlier, quiet now: neither delivering nor missing.
            SilenceVerdict::Exceeded => {
                mark_recovered(entry);
                return true;
            }
        }
        if now.saturating_duration_since(entry.recorded_at)
            < Duration::from_secs(RECONCILE_DELTA_SECS)
        {
            return true;
        }
        self.missing = self.missing.saturating_add(1);
        if entry.rounds >= RECONCILE_MAX_ROUNDS {
            // The last round gets its full 120 s after it finished before it
            // is judged spent.
            let last_round_settled = socket.in_flight.is_none()
                && socket.last_round_at.is_none_or(|at| {
                    now.saturating_duration_since(at) >= Duration::from_secs(RECONCILE_DELTA_SECS)
                });
            if last_round_settled && !entry.exhausted_reported {
                entry.exhausted_reported = true;
                self.exhausted = self.exhausted.saturating_add(1);
                if inner.exhausted_named.len() < NAMED_IN_LOG {
                    inner.exhausted_named.push(entry.instrument);
                }
            }
            return true;
        }
        if socket.candidates.len() < RECONCILE_MAX_PER_SOCKET_PER_ROUND {
            socket.candidates.push(entry.instrument);
        }
        true
    }

    /// [`Self::note`] for a report the silence detector handed out with its
    /// slot index (`TickGapDetector::scan_silence_slots`), 2026-11-17.
    ///
    /// **Fast path, O(1) with no hash:** when no round has been reserved today
    /// (`rounds_any` clear, so every entry has `rounds == 0`) and the slot's
    /// cell is valid and holds this report's key, the verdict is judged from
    /// the cell alone: `Live` and a recent `Warming` count the socket as
    /// delivering, `Exceeded` does nothing, and a `NeverTicked` recorded at
    /// least [`RECONCILE_DELTA_SECS`] ago counts as missing and joins its
    /// socket's candidates. That is exactly what `note` does for an entry with
    /// `rounds == 0` (its `mark_recovered` and exhausted branch are no-ops
    /// then), in the same order, with the same values: pinned by
    /// `proptest_note_at_slot_matches_note`.
    ///
    /// **Slow path:** anything else (no cell yet, another key in the cell, a
    /// slot past [`RECONCILE_CELL_SLOTS`], or a round reserved today) runs
    /// `note` unchanged, then, while no round is reserved, caches the entry in
    /// this slot's cell for the next sweep (one more probe, and one for a
    /// different key the cell held, rare). So the first sweep after a
    /// subscribe pays the probe and every later one reads the dense cells. A
    /// key the book does not expect (refused past the cap, or forgotten while
    /// the detector still tracks it) gets no cell and costs its one probe on
    /// every sweep, as before the cells.
    ///
    /// Each fast-path report is counted ([`SubscribeReconcile::fast_path_hits`])
    /// so a test can prove the fast path ran, not only that the cells are valid.
    ///
    /// No allocation on either path (the candidate list is pre-sized).
    pub fn note_at_slot(&mut self, slot: u32, report: &SilenceReport) {
        let now = self.now;
        let inner = &mut *self.inner;
        let cell = if inner.rounds_any {
            None
        } else {
            usize::try_from(slot)
                .ok()
                .and_then(|index| inner.cells.get(index))
                .copied()
                .filter(|cell| {
                    cell.socket != CELL_EMPTY
                        && cell.security_id == report.key.0
                        && cell.segment == report.key.1
                })
        };
        if let Some(cell) = cell {
            self.fast_hits = self.fast_hits.saturating_add(1);
            let Some(socket) = inner.sockets.get_mut(usize::from(cell.socket)) else {
                return;
            };
            match report.verdict {
                SilenceVerdict::Live => {
                    socket.delivering = socket.delivering.saturating_add(1);
                }
                SilenceVerdict::Warming => {
                    if report.silent_millis <= report.expected_millis {
                        socket.delivering = socket.delivering.saturating_add(1);
                    }
                }
                SilenceVerdict::Exceeded => {}
                SilenceVerdict::NeverTicked => {
                    if now.saturating_duration_since(cell.recorded_at)
                        < Duration::from_secs(RECONCILE_DELTA_SECS)
                    {
                        return;
                    }
                    self.missing = self.missing.saturating_add(1);
                    if socket.candidates.len() < RECONCILE_MAX_PER_SOCKET_PER_ROUND {
                        socket.candidates.push(SubscribeInstrument {
                            security_id: cell.security_id,
                            segment: cell.segment,
                        });
                    }
                }
            }
            return;
        }
        // A key the book does not expect never gets a cell: one probe, not two.
        if self.note(report) {
            self.cache_slot(slot, report.key);
        }
    }

    /// The slow path's second half: puts `key`'s entry in `slot`'s cell while
    /// no round is reserved today. Keeps the invariant both ways: the cell the
    /// entry held before is emptied, and an entry the cell held before loses
    /// its slot. O(1), at most two probes.
    fn cache_slot(&mut self, slot: u32, key: Key) {
        let inner = &mut *self.inner;
        if inner.rounds_any {
            return;
        }
        let Some(index) = usize::try_from(slot)
            .ok()
            .filter(|index| *index < inner.cells.len())
        else {
            return;
        };
        let Inner {
            expected, cells, ..
        } = inner;
        let Some(entry) = expected.get_mut(&key) else {
            return;
        };
        let previous_slot = entry.view_slot;
        entry.view_slot = slot;
        let fresh = SlotCell {
            security_id: key.0,
            recorded_at: entry.recorded_at,
            segment: key.1,
            socket: entry.socket,
        };
        if previous_slot != VIEW_SLOT_NONE
            && previous_slot != slot
            && let Some(old) = usize::try_from(previous_slot)
                .ok()
                .and_then(|old| cells.get_mut(old))
        {
            old.socket = CELL_EMPTY;
        }
        let Some(cell) = cells.get_mut(index) else {
            return;
        };
        if cell.socket != CELL_EMPTY
            && (cell.security_id, cell.segment) != key
            && let Some(other) = expected.get_mut(&(cell.security_id, cell.segment))
            && other.view_slot == slot
        {
            other.view_slot = VIEW_SLOT_NONE;
        }
        *cell = fresh;
    }

    /// Ends the pass at the wall clock, with the trading-calendar session gate
    /// the frame-silence redials use.
    pub fn finish_now(self) -> SweepSummary {
        let secs = tickvault_core::websocket::silence_calendar::session_secs_of_day_now();
        self.finish_at(secs, ist_day_now())
    }

    /// Ends the pass at `secs_of_day_ist` on IST day `ist_day`.
    pub fn finish_at(mut self, secs_of_day_ist: u32, ist_day: i64) -> SweepSummary {
        self.inner.fast_path_hits = self
            .inner
            .fast_path_hits
            .saturating_add(u64::from(self.fast_hits));
        let mut summary = SweepSummary {
            missing: self.missing,
            newly_exhausted: self.exhausted,
            ..SweepSummary::default()
        };
        gauge!(RECONCILE_MISSING_GAUGE).set(f64::from(self.missing));
        let inner = &mut *self.inner;
        if inner.ist_day != ist_day {
            // A new session: every instrument gets its rounds back. O(expected),
            // once a day. A round still in flight keeps its socket's flag; the
            // executor clears it.
            inner.ist_day = ist_day;
            inner.feed_silent_reported = false;
            inner.rounds_any = false;
            for entry in inner.expected.values_mut() {
                entry.rounds = 0;
                entry.reserved_round = None;
                entry.missing_counted = false;
                entry.exhausted_reported = false;
                entry.recovered_reported = false;
            }
            for socket in &mut inner.sockets {
                socket.last_round_at = None;
                socket.silent_reported = false;
            }
            self.exhausted = 0;
            inner.exhausted_named.clear();
            summary.newly_exhausted = 0;
        }
        // The continuous session, and not its first 120 s: an instrument that has
        // not traded since the open is not yet a lost subscription.
        if !reconcile_window_open(secs_of_day_ist) {
            return summary;
        }
        let feed_delivering: u64 = inner.sockets.iter().map(|s| u64::from(s.delivering)).sum();
        if feed_delivering == 0 {
            if summary.missing > 0 && !inner.feed_silent_reported {
                inner.feed_silent_reported = true;
                counter!(RECONCILE_ROUNDS_METRIC, "outcome" => "skipped_feed_silent").increment(1);
                warn!(
                    missing = summary.missing,
                    "subscribe reconcile: no subscribed instrument is delivering now - this is a \
                     halt or a dead feed, not a partial subscribe; nothing is resent"
                );
            }
            return summary;
        }
        inner.feed_silent_reported = false;
        let interval = Duration::from_secs(RECONCILE_ROUND_INTERVAL_SECS);
        let auto = self.book.auto_resubscribe;
        let Inner {
            sockets,
            expected,
            next_reservation,
            rounds_any,
            ..
        } = inner;
        for (index, socket) in sockets.iter_mut().enumerate() {
            if socket.candidates.is_empty() {
                socket.silent_reported = false;
                continue;
            }
            if socket.delivering == 0 {
                if !socket.silent_reported {
                    socket.silent_reported = true;
                    counter!(RECONCILE_ROUNDS_METRIC, "outcome" => "skipped_silent_socket")
                        .increment(1);
                    warn!(
                        socket = index,
                        missing = socket.candidates.len(),
                        "subscribe reconcile: a socket delivers nothing now for any of its \
                         instruments - left to the frame-silence redial, nothing is resent"
                    );
                }
                continue;
            }
            socket.silent_reported = false;
            if socket.in_flight.is_some()
                || socket
                    .last_round_at
                    .is_some_and(|at| self.now.saturating_duration_since(at) < interval)
            {
                continue;
            }
            // `drain` keeps the pre-sized buffer for the next sweep; the round
            // takes one allocation, cold, at most once per socket per 120 s.
            let instruments: Vec<SubscribeInstrument> = socket.candidates.drain(..).collect();
            let count = instruments.len() as u64;
            let first_seen = count_missing_once(expected, &instruments);
            counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "missing_found")
                .increment(first_seen);
            if auto {
                // One round spent per member, one hash probe each; refunded by
                // the executor for any member it did not actually resend.
                let Some(reservation) = next_reservation.checked_add(1) else {
                    counter!(RECONCILE_ROUNDS_METRIC, "outcome" => "round_id_exhausted")
                        .increment(1);
                    continue;
                };
                *next_reservation = reservation;
                reserve_round(expected, rounds_any, &instruments, reservation);
                let round = PlannedRound {
                    tx: socket.tx.clone(),
                    socket: u8::try_from(index).unwrap_or(u8::MAX),
                    instruments,
                    reservation,
                    ist_day,
                };
                match self.book.rounds_tx.try_send(round) {
                    Ok(()) => {
                        socket.in_flight = Some(reservation);
                        summary.dispatched = summary.dispatched.saturating_add(1);
                        counter!(RECONCILE_ROUNDS_METRIC, "outcome" => "dispatched").increment(1);
                    }
                    Err(
                        mpsc::error::TrySendError::Full(round)
                        | mpsc::error::TrySendError::Closed(round),
                    ) => {
                        // Not a spent round: the instruments get theirs back
                        // and the socket keeps its spacing.
                        refund_reservation(expected, &round.instruments, reservation);
                        counter!(RECONCILE_ROUNDS_METRIC, "outcome" => "queue_full").increment(1);
                        continue;
                    }
                }
            } else {
                // Shadow: spacing only, so the counter reads one decision per
                // socket per 120 s; no round is spent.
                socket.last_round_at = Some(self.now);
                summary.would_resubscribe = summary.would_resubscribe.saturating_add(1);
                counter!(RECONCILE_ROUNDS_METRIC, "outcome" => "would_resubscribe").increment(1);
            }
            info!(
                socket = index,
                instruments = count,
                auto_resubscribe = auto,
                "subscribe reconcile: instruments subscribed over 120 s ago have sent nothing \
                 while the socket delivers others"
            );
        }
        summary
    }
}

impl Drop for ReconcileSweep<'_> {
    /// Reports instruments that used their last round, once each, whether or
    /// not the pass reached `finish` (a skipped gate must not hide them).
    fn drop(&mut self) {
        if self.exhausted == 0 {
            return;
        }
        let n = self.exhausted;
        counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "exhausted").increment(u64::from(n));
        warn!(
            code = ErrorCode::WsGapSubscriptionBatching.code_str(),
            source = "subscribe_reconcile_exhausted",
            instruments = n,
            first = %NamedInstruments(&self.inner.exhausted_named),
            "subscribe reconcile: instruments still sent nothing after the last resend round - \
             either they do not trade or the vendor is not serving them; not resent again today"
        );
    }
}

/// Empties the cell `entry` is cached in, if any, and clears its slot. Called
/// under the book's lock wherever an entry's socket or record time changes or
/// the entry leaves the expected set, so no cell ever serves a stale copy.
/// O(1).
fn release_cell(cells: &mut [SlotCell], entry: &mut Expected) {
    if entry.view_slot != VIEW_SLOT_NONE {
        if let Some(cell) = usize::try_from(entry.view_slot)
            .ok()
            .and_then(|slot| cells.get_mut(slot))
        {
            cell.socket = CELL_EMPTY;
        }
        entry.view_slot = VIEW_SLOT_NONE;
    }
}

/// Records first-packet observation after a resend reservation. This is temporal
/// evidence only: the reservation may still be queued, so it does not prove the
/// resend caused recovery or that its wire commands completed.
fn mark_recovered(entry: &mut Expected) {
    if entry.rounds > 0 && !entry.recovered_reported {
        entry.recovered_reported = true;
        counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "recovered").increment(1);
    }
}

/// Reserves one of the daily rounds for this unique queued operation.
fn reserve_round(
    expected: &mut ExpectedMap,
    rounds_any: &mut bool,
    instruments: &[SubscribeInstrument],
    reservation: u64,
) {
    // From here to the day reset an entry may hold `rounds > 0`, so the
    // sweep's fast path stands down and every report takes the probe.
    *rounds_any = true;
    for instrument in instruments {
        if let Some(entry) = expected.get_mut(&key_of(instrument)) {
            entry.rounds = entry.rounds.saturating_add(1).min(RECONCILE_MAX_ROUNDS);
            entry.reserved_round = Some(reservation);
        }
    }
}

/// Refund only this still-current reservation, at most once. The caller also
/// verifies its IST day; forget/rebind/day-reset invalidate the identity.
fn refund_reservation(
    expected: &mut ExpectedMap,
    instruments: &[SubscribeInstrument],
    reservation: u64,
) {
    for instrument in instruments {
        if let Some(entry) = expected.get_mut(&key_of(instrument))
            && entry.reserved_round == Some(reservation)
        {
            entry.rounds = entry.rounds.saturating_sub(1);
            entry.reserved_round = None;
        }
    }
}

/// Marks instruments as counted missing today; returns how many were new.
fn count_missing_once(expected: &mut ExpectedMap, instruments: &[SubscribeInstrument]) -> u64 {
    let mut fresh = 0u64;
    for instrument in instruments {
        if let Some(entry) = expected.get_mut(&key_of(instrument))
            && !entry.missing_counted
        {
            entry.missing_counted = true;
            fresh = fresh.saturating_add(1);
        }
    }
    fresh
}

/// Up to [`NAMED_IN_LOG`] instruments as `segment:id`, for one log line.
struct NamedInstruments<'a>(&'a [SubscribeInstrument]);

impl std::fmt::Display for NamedInstruments<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, instrument) in self.0.iter().take(NAMED_IN_LOG).enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            write!(
                f,
                "{}:{}",
                instrument.segment.as_str(),
                instrument.security_id
            )?;
        }
        if self.0.len() > NAMED_IN_LOG {
            write!(f, ",+{} more", self.0.len() - NAMED_IN_LOG)?;
        }
        Ok(())
    }
}

/// What one executed round ended as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoundOutcome {
    /// Every instrument that left the socket was subscribed again (or handed to
    /// its reconnect replay); not-held ones were placed where there was room.
    Resubscribed {
        /// Instruments put on the socket by the subscribe commands.
        resent: usize,
        /// Instruments the socket did not hold at all (placed or not).
        not_held: usize,
        /// Not-held instruments left unplaced by refusal or stale eligibility.
        unplaced: usize,
    },
    /// The socket refused every change after an 805; nothing changed.
    Refused {
        /// A `ResubscribeOutcome::REASON_*`.
        reason: &'static str,
    },
    /// The socket's task could not be reached; nothing changed.
    SendFailed,
    /// The reservation became stale before changing any subscription.
    Canceled,
    /// The subscribe command was handed over and no answer came back: these
    /// may or may not be on the socket.
    Unknown(Vec<SubscribeInstrument>),
    /// Instruments the subscribe half could not put back after every attempt.
    Lost(Vec<SubscribeInstrument>),
}

/// Runs planned rounds until every strong handle to the book is dropped.
/// Spawn once beside the lane with `Arc::downgrade(&book)`: the executor holds
/// only a weak handle, so dropping the book closes the round queue and ends
/// this loop.
pub async fn run_reconcile_executor(
    book: std::sync::Weak<SubscribeReconcile>,
    rounds: mpsc::Receiver<PlannedRound>,
) {
    run_executor_with_clock(book, rounds, execution_clock).await;
}

fn execution_clock() -> (u32, i64) {
    (
        tickvault_core::websocket::silence_calendar::session_secs_of_day_now(),
        ist_day_now(),
    )
}

async fn run_executor_with_clock<C>(
    book: std::sync::Weak<SubscribeReconcile>,
    mut rounds: mpsc::Receiver<PlannedRound>,
    mut clock: C,
) where
    C: FnMut() -> (u32, i64),
{
    while let Some(round) = rounds.recv().await {
        let Some(book) = book.upgrade() else {
            break;
        };
        let socket = round.socket;
        match execute_round_with_clock(&book, round, &mut clock).await {
            RoundOutcome::Lost(lost) => error!(
                code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                source = "subscribe_reconcile_lost",
                socket,
                instruments = lost.len(),
                first = %NamedInstruments(&lost),
                "subscribe reconcile: instruments were unsubscribed for a resend and could not \
                 be subscribed again - they are off this socket until the next attach or redial"
            ),
            RoundOutcome::Unknown(unknown) => warn!(
                code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                source = "subscribe_reconcile_unknown",
                socket,
                instruments = unknown.len(),
                first = %NamedInstruments(&unknown),
                "subscribe reconcile: the socket never answered the subscribe half - these may be \
                 off the socket; the next sweep judges them again"
            ),
            _ => {}
        }
    }
}

/// Restoration must run even after eligibility changes; placing a member
/// known not to be held is new work and uses the same fresh gate as removal.
#[derive(Clone, Copy)]
enum RoundCommand {
    Unsubscribe,
    Restore,
    PlaceNotHeld,
}

impl RoundCommand {
    fn command(
        self,
        instruments: &[SubscribeInstrument],
        ack: oneshot::Sender<ResubscribeOutcome>,
    ) -> LiveSubscriptionCommand {
        let (unsubscribe, subscribe) = match self {
            Self::Unsubscribe => (instruments.to_vec(), Vec::new()),
            Self::Restore | Self::PlaceNotHeld => (Vec::new(), instruments.to_vec()),
        };
        LiveSubscriptionCommand::Resubscribe {
            unsubscribe,
            subscribe,
            ack: Some(ack),
        }
    }
}

enum CommandAttempt {
    Canceled,
    Reply(Option<Option<ResubscribeOutcome>>),
}

async fn dispatch_reconcile_checked<V>(
    tx: &mpsc::Sender<LiveSubscriptionCommand>,
    instruments: &[SubscribeInstrument],
    kind: RoundCommand,
    checked_send: &mut V,
) -> CommandAttempt
where
    V: for<'permit> FnMut(
        &[SubscribeInstrument],
        RoundCommand,
        mpsc::Permit<'permit, LiveSubscriptionCommand>,
        oneshot::Sender<ResubscribeOutcome>,
    ) -> bool,
{
    let Ok(Ok(permit)) = tokio::time::timeout(
        Duration::from_secs(RECONCILE_SEND_TIMEOUT_SECS),
        tx.reserve(),
    )
    .await
    else {
        return CommandAttempt::Reply(None);
    };
    let (ack, answer) = oneshot::channel();
    if !checked_send(instruments, kind, permit, ack) {
        return CommandAttempt::Canceled;
    }
    CommandAttempt::Reply(Some(
        tokio::time::timeout(Duration::from_secs(RECONCILE_ACK_TIMEOUT_SECS), answer)
            .await
            .ok()
            .and_then(Result::ok),
    ))
}

/// `all` minus `minus`, on the composite key. O(all + minus), cold.
fn without(
    all: Vec<SubscribeInstrument>,
    minus: &[SubscribeInstrument],
) -> Vec<SubscribeInstrument> {
    if minus.is_empty() {
        return all;
    }
    let drop: HashSet<Key> = minus.iter().map(key_of).collect();
    all.into_iter()
        .filter(|instrument| !drop.contains(&key_of(instrument)))
        .collect()
}

/// What the unsubscribe half did to each instrument.
#[derive(Debug, Default)]
struct UnsubscribeHalf {
    /// Left the socket (or the socket is being replaced without them).
    removed: Vec<SubscribeInstrument>,
    /// The socket did not hold them.
    not_held: Vec<SubscribeInstrument>,
    /// Handed over, never answered: treated as removed.
    unknown: Vec<SubscribeInstrument>,
    /// Never touched (send failed, stopped before them, cap reached): still on
    /// the socket, not resent, refunded.
    untouched: Vec<SubscribeInstrument>,
    /// The socket refused every change after an 805.
    halted: bool,
    /// The very first command could not be handed over.
    unreachable: bool,
    /// At least one not-yet-sent batch became stale before enqueue.
    canceled: bool,
}

/// The unsubscribe half: removes the round's instruments from the socket,
/// splitting any batch the socket refuses as not held until each refused part
/// is one instrument. Bounded by [`RECONCILE_MAX_UNSUBSCRIBE_COMMANDS`].
async fn unsubscribe_half<V>(
    tx: &mpsc::Sender<LiveSubscriptionCommand>,
    instruments: Vec<SubscribeInstrument>,
    checked_send: &mut V,
) -> UnsubscribeHalf
where
    V: for<'permit> FnMut(
        &[SubscribeInstrument],
        RoundCommand,
        mpsc::Permit<'permit, LiveSubscriptionCommand>,
        oneshot::Sender<ResubscribeOutcome>,
    ) -> bool,
{
    let count_round = |outcome: &'static str| {
        counter!(RECONCILE_ROUNDS_METRIC, "outcome" => outcome).increment(1);
    };
    let mut half = UnsubscribeHalf::default();
    let mut work: Vec<Vec<SubscribeInstrument>> = vec![instruments];
    let mut commands = 0usize;
    let mut cap_reported = false;
    while let Some(batch) = work.pop() {
        if half.halted || commands >= RECONCILE_MAX_UNSUBSCRIBE_COMMANDS {
            if !half.halted && !cap_reported {
                cap_reported = true;
                count_round("split_cap_reached");
            }
            half.untouched.extend(batch);
            continue;
        }
        commands += 1;
        let reply =
            match dispatch_reconcile_checked(tx, &batch, RoundCommand::Unsubscribe, checked_send)
                .await
            {
                CommandAttempt::Canceled => {
                    half.canceled = true;
                    half.untouched.extend(batch);
                    continue;
                }
                CommandAttempt::Reply(reply) => reply,
            };
        match reply {
            None => {
                if commands == 1 {
                    half.unreachable = true;
                }
                half.untouched.extend(batch);
            }
            Some(Some(ResubscribeOutcome::Applied | ResubscribeOutcome::ReplayPending)) => {
                half.removed.extend(batch);
            }
            Some(Some(ResubscribeOutcome::Stopped {
                not_unsubscribed, ..
            })) => {
                half.untouched.extend_from_slice(&not_unsubscribed);
                half.removed.extend(without(batch, &not_unsubscribed));
            }
            Some(Some(ResubscribeOutcome::Refused { reason }))
                if reason == ResubscribeOutcome::REASON_NOT_HELD =>
            {
                if batch.len() == 1 {
                    half.not_held.extend(batch);
                } else {
                    count_round("unsubscribe_split");
                    let mut low = batch;
                    let high = low.split_off(low.len() / 2);
                    work.push(high);
                    work.push(low);
                }
            }
            // Over cap cannot come from an unsubscribe-only command; an 805
            // refuses everything that follows, so stop sending.
            Some(Some(ResubscribeOutcome::Refused { .. })) => {
                half.halted = true;
                half.untouched.extend(batch);
            }
            Some(None) => {
                count_round("unsubscribe_unknown");
                half.unknown.extend(batch);
            }
        }
    }
    half
}

/// Executes one round on the round's socket: unsubscribe, then subscribe what
/// left (plus what the socket did not hold). Cold, on the executor's task.
/// Always reports back to the book, which clears the socket's in-flight flag
/// and refunds each untouched matching current-day reservation. A rebind or
/// forgotten/recreated entry does not inherit a previous reservation refund.
async fn execute_round_with_clock<C>(
    book: &SubscribeReconcile,
    round: PlannedRound,
    clock: &mut C,
) -> RoundOutcome
where
    C: FnMut() -> (u32, i64),
{
    execute_round_on_checked(
        &round.tx,
        round.instruments.clone(),
        |refund| book.round_finished(&round, refund, Instant::now()),
        |instruments, kind, permit, ack| {
            book.send_round_change(&round, instruments, kind, permit, ack, clock)
        },
    )
    .await
}

#[cfg(test)]
async fn execute_round_on<F>(
    tx: &mpsc::Sender<LiveSubscriptionCommand>,
    instruments: Vec<SubscribeInstrument>,
    finished: F,
) -> RoundOutcome
where
    F: FnOnce(&[SubscribeInstrument]),
{
    execute_round_on_checked(tx, instruments, finished, |members, kind, permit, ack| {
        permit.send(kind.command(members, ack));
        true
    })
    .await
}

async fn execute_round_on_checked<F, V>(
    tx: &mpsc::Sender<LiveSubscriptionCommand>,
    instruments: Vec<SubscribeInstrument>,
    finished: F,
    mut checked_send: V,
) -> RoundOutcome
where
    F: FnOnce(&[SubscribeInstrument]),
    V: for<'permit> FnMut(
        &[SubscribeInstrument],
        RoundCommand,
        mpsc::Permit<'permit, LiveSubscriptionCommand>,
        oneshot::Sender<ResubscribeOutcome>,
    ) -> bool,
{
    let count_round = |outcome: &'static str| {
        counter!(RECONCILE_ROUNDS_METRIC, "outcome" => outcome).increment(1);
    };
    let half = unsubscribe_half(tx, instruments, &mut checked_send).await;
    if half.canceled
        && half.removed.is_empty()
        && half.unknown.is_empty()
        && half.not_held.is_empty()
    {
        count_round("canceled_stale");
        finished(&half.untouched);
        return RoundOutcome::Canceled;
    }
    if half.unreachable && half.removed.is_empty() && half.unknown.is_empty() {
        count_round("send_failed");
        let mut refund = half.untouched;
        refund.extend(half.not_held);
        finished(&refund);
        return RoundOutcome::SendFailed;
    }
    if half.halted && half.removed.is_empty() && half.unknown.is_empty() {
        count_round("refused_halted_805");
        let mut refund = half.untouched;
        refund.extend(half.not_held);
        finished(&refund);
        return RoundOutcome::Refused {
            reason: ResubscribeOutcome::REASON_HALTED_805,
        };
    }
    let not_held_list = half.not_held;
    let not_held = not_held_list.len();
    counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "not_held").increment(not_held as u64);
    // Three subscribe phases, so a group that may need room nobody freed never
    // fails one that freed its own:
    // 1. `removed`: this round took them off, each freed its own slot, so they
    //    fit whatever the socket's cap; one still off at the end is `lost`;
    // 2. `unknown`: the unsubscribe got no answer, so each may still be held
    //    (a duplicate, dropped by the socket) or may need a slot; one not
    //    placed is `unknown`, never `lost`;
    // 3. `not_held`: never on the socket; this is new placement, so each
    //    attempt rechecks eligibility after obtaining channel capacity. A
    //    stale tail is refunded; one that finds no room is `unplaceable`.
    //    Runs only when 1 and 2 placed everything.
    let removed_len = half.removed.len();
    let unknown_len = half.unknown.len();
    if removed_len == 0 && unknown_len == 0 && not_held == 0 {
        finished(&half.untouched);
        return RoundOutcome::Resubscribed {
            resent: 0,
            not_held: 0,
            unplaced: 0,
        };
    }
    let mut refund = half.untouched;
    let lost =
        match subscribe_with_retries(tx, half.removed, RoundCommand::Restore, &mut checked_send)
            .await
        {
            SubscribeEnd::Done => Vec::new(),
            SubscribeEnd::Remaining(remaining) | SubscribeEnd::Canceled(remaining) => remaining,
            SubscribeEnd::Unknown(mut unanswered) => {
                counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "resent")
                    .increment(removed_len.saturating_sub(unanswered.len()) as u64);
                // Nothing after phase one was tried: the not-held ones get their
                // round back; the unknown ones may be off the socket, so they are
                // reported with the unanswered ones.
                counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "unknown")
                    .increment(unknown_len as u64);
                unanswered.extend(half.unknown);
                refund.extend(not_held_list);
                finished(&refund);
                return RoundOutcome::Unknown(unanswered);
            }
        };
    let mut resent = removed_len.saturating_sub(lost.len());
    // A refusal is counted here; a missing answer is counted inside the helper.
    let mut uncertain_refused = false;
    let uncertain =
        match subscribe_with_retries(tx, half.unknown, RoundCommand::Restore, &mut checked_send)
            .await
        {
            SubscribeEnd::Done => Vec::new(),
            SubscribeEnd::Remaining(remaining) | SubscribeEnd::Canceled(remaining) => {
                uncertain_refused = true;
                counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "unknown")
                    .increment(remaining.len() as u64);
                remaining
            }
            SubscribeEnd::Unknown(unanswered) => unanswered,
        };
    resent = resent.saturating_add(unknown_len.saturating_sub(uncertain.len()));
    let mut unplaced = 0;
    let mut placement_canceled = false;
    if !lost.is_empty() || !uncertain.is_empty() {
        // The socket had no room, or did not answer: placing more would compete
        // for the same room or send blind. Not tried, so refunded.
        refund.extend(not_held_list);
    } else if !not_held_list.is_empty() {
        match subscribe_with_retries(
            tx,
            not_held_list,
            RoundCommand::PlaceNotHeld,
            &mut checked_send,
        )
        .await
        {
            SubscribeEnd::Done => resent = resent.saturating_add(not_held),
            SubscribeEnd::Canceled(remaining) => {
                placement_canceled = true;
                unplaced = remaining.len();
                resent = resent.saturating_add(not_held.saturating_sub(unplaced));
                refund.extend(remaining);
                count_round("canceled_stale");
            }
            SubscribeEnd::Remaining(remaining) => {
                unplaced = remaining.len();
                resent = resent.saturating_add(not_held.saturating_sub(unplaced));
                counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "unplaceable")
                    .increment(unplaced as u64);
                warn!(
                    code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                    source = "subscribe_reconcile_unplaceable",
                    instruments = unplaced,
                    first = %NamedInstruments(&remaining),
                    "subscribe reconcile: expected instruments the socket does not hold \
                     found no room on it after every attempt - nothing else was taken off"
                );
            }
            SubscribeEnd::Unknown(unanswered) => {
                counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "resent").increment(
                    resent.saturating_add(not_held.saturating_sub(unanswered.len())) as u64,
                );
                finished(&refund);
                return RoundOutcome::Unknown(unanswered);
            }
        }
    }
    counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "resent").increment(resent as u64);
    finished(&refund);
    if placement_canceled && resent == 0 && removed_len == 0 && unknown_len == 0 {
        RoundOutcome::Canceled
    } else if !lost.is_empty() {
        if !uncertain.is_empty() {
            // The outcome carries the lost ones; name the uncertain ones here
            // so none is off the socket without a report.
            warn!(
                code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                source = "subscribe_reconcile_unknown",
                instruments = uncertain.len(),
                first = %NamedInstruments(&uncertain),
                "subscribe reconcile: the socket did not take these back in the same round \
                 that lost others - they may be off the socket; the next sweep judges them again"
            );
        }
        count_round("lost");
        counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "lost").increment(lost.len() as u64);
        RoundOutcome::Lost(lost)
    } else if !uncertain.is_empty() {
        if uncertain_refused {
            count_round("subscribe_unknown");
        }
        RoundOutcome::Unknown(uncertain)
    } else {
        count_round("resubscribed");
        RoundOutcome::Resubscribed {
            resent,
            not_held,
            unplaced,
        }
    }
}

/// How one subscribe phase of a round ended.
#[derive(Debug, PartialEq, Eq)]
enum SubscribeEnd {
    /// Every instrument is on the socket (or in its reconnect replay).
    Done,
    /// New placement was canceled before enqueue because it became stale.
    Canceled(Vec<SubscribeInstrument>),
    /// These were still not placed after every attempt.
    Remaining(Vec<SubscribeInstrument>),
    /// The command was handed over and no answer came back.
    Unknown(Vec<SubscribeInstrument>),
}

/// Subscribes `pending` on the socket, retrying only on a definite failure
/// (refused, or not handed over) up to [`RECONCILE_SUBSCRIBE_ATTEMPTS`]; a
/// missing answer ends it as unknown rather than resending blindly.
async fn subscribe_with_retries<V>(
    tx: &mpsc::Sender<LiveSubscriptionCommand>,
    mut pending: Vec<SubscribeInstrument>,
    kind: RoundCommand,
    checked_send: &mut V,
) -> SubscribeEnd
where
    V: for<'permit> FnMut(
        &[SubscribeInstrument],
        RoundCommand,
        mpsc::Permit<'permit, LiveSubscriptionCommand>,
        oneshot::Sender<ResubscribeOutcome>,
    ) -> bool,
{
    if pending.is_empty() {
        return SubscribeEnd::Done;
    }
    for attempt in 0..RECONCILE_SUBSCRIBE_ATTEMPTS {
        if attempt > 0 {
            counter!(RECONCILE_ROUNDS_METRIC, "outcome" => "subscribe_retried").increment(1);
            tokio::time::sleep(Duration::from_secs(RECONCILE_SUBSCRIBE_RETRY_SECS)).await;
        }
        let reply = match dispatch_reconcile_checked(tx, &pending, kind, checked_send).await {
            CommandAttempt::Canceled => return SubscribeEnd::Canceled(pending),
            CommandAttempt::Reply(reply) => reply,
        };
        match reply {
            Some(Some(ResubscribeOutcome::Applied | ResubscribeOutcome::ReplayPending)) => {
                return SubscribeEnd::Done;
            }
            Some(Some(ResubscribeOutcome::Stopped { not_subscribed, .. })) => {
                pending = not_subscribed;
                if pending.is_empty() {
                    return SubscribeEnd::Done;
                }
            }
            Some(None) => {
                counter!(RECONCILE_ROUNDS_METRIC, "outcome" => "subscribe_unknown").increment(1);
                counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "unknown")
                    .increment(pending.len() as u64);
                return SubscribeEnd::Unknown(pending);
            }
            // Refused (over cap: another change filled the room) or not handed
            // over: try again after the pause.
            Some(Some(ResubscribeOutcome::Refused { .. })) | None => {}
        }
    }
    SubscribeEnd::Remaining(pending)
}

/// Today's IST day number since the Unix epoch.
#[must_use]
pub fn ist_day_now() -> i64 {
    chrono::Utc::now()
        .timestamp()
        .saturating_add(IST_UTC_OFFSET_SECONDS_I64)
        .div_euclid(i64::from(SECONDS_PER_DAY))
}
#[cfg(test)]
mod tests {
    use super::*;

    /// 10:00 IST: inside the continuous session and past its first 120 s.
    const IN_SESSION: u32 = 10 * 3600;
    const DAY: i64 = 20_000;

    fn si(id: u64) -> SubscribeInstrument {
        SubscribeInstrument {
            security_id: id,
            segment: ExchangeSegment::NseFno,
        }
    }

    fn sis(ids: &[u64]) -> Vec<SubscribeInstrument> {
        ids.iter().copied().map(si).collect()
    }

    fn report(id: u64, verdict: SilenceVerdict) -> SilenceReport {
        SilenceReport {
            key: (id, ExchangeSegment::NseFno),
            silent_millis: 0,
            expected_millis: 0,
            baseline_millis: 0,
            samples: 0,
            verdict,
            sparse: false,
            warming_up: false,
            alarm_sparse: false,
        }
    }

    fn socket() -> (
        mpsc::Sender<LiveSubscriptionCommand>,
        mpsc::Receiver<LiveSubscriptionCommand>,
    ) {
        mpsc::channel(64)
    }

    fn one_sweep(
        book: &SubscribeReconcile,
        at: Instant,
        secs: u32,
        delivering: &[u64],
        missing: &[u64],
    ) -> SweepSummary {
        one_sweep_on(book, at, secs, DAY, delivering, missing)
    }

    fn one_sweep_on(
        book: &SubscribeReconcile,
        at: Instant,
        secs: u32,
        day: i64,
        delivering: &[u64],
        missing: &[u64],
    ) -> SweepSummary {
        let mut sweep = book.sweep_at(at);
        for id in delivering {
            sweep.note(&report(*id, SilenceVerdict::Live));
        }
        for id in missing {
            sweep.note(&report(*id, SilenceVerdict::NeverTicked));
        }
        sweep.finish_at(secs, day)
    }

    /// Stands in for the executor: takes every queued round and reports it
    /// finished at `at`, nothing refunded. Returns how many it took.
    fn finish_queued(
        book: &SubscribeReconcile,
        rx: &mut mpsc::Receiver<PlannedRound>,
        at: Instant,
    ) -> usize {
        let mut n = 0;
        while let Ok(round) = rx.try_recv() {
            book.round_finished(&round, &[], at);
            n += 1;
        }
        n
    }

    fn delta() -> Duration {
        Duration::from_secs(RECONCILE_DELTA_SECS)
    }

    fn entry(book: &SubscribeReconcile, id: u64) -> Expected {
        book.lock().expected[&(id, ExchangeSegment::NseFno)]
    }

    #[test]
    fn test_sweep_finish_at_counts_missing_only_after_the_delta() {
        let (book, _rx) = SubscribeReconcile::new(false);
        let (tx, _sock) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &sis(&[1, 2]), t0);
        let early = one_sweep(
            &book,
            t0 + delta() - Duration::from_secs(1),
            IN_SESSION,
            &[1],
            &[2],
        );
        assert_eq!(
            early.missing, 0,
            "one second before the delta is not missing yet"
        );
        let late = one_sweep(&book, t0 + delta(), IN_SESSION, &[1], &[2]);
        assert_eq!(late.missing, 1);
    }

    #[test]
    fn test_shadow_mode_counts_sends_nothing_and_spends_no_round() {
        let (book, mut rx) = SubscribeReconcile::new(false);
        let (tx, _sock) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &sis(&[1, 2]), t0);
        let mut at = t0 + delta();
        let mut would = 0;
        let mut exhausted = 0;
        // 30 s sweeps for 20 minutes.
        for _ in 0..40 {
            let s = one_sweep(&book, at, IN_SESSION, &[1], &[2]);
            assert_eq!(s.dispatched, 0);
            would += s.would_resubscribe;
            exhausted += s.newly_exhausted;
            at += Duration::from_secs(30);
        }
        assert!(
            rx.try_recv().is_err(),
            "shadow mode hands nothing to the executor"
        );
        assert_eq!(
            would, 10,
            "one decision per socket per 120 s over 20 minutes"
        );
        assert_eq!(
            exhausted, 0,
            "nothing was resent, so nothing is reported exhausted"
        );
        let e = entry(&book, 2);
        assert_eq!(e.rounds, 0, "shadow mode never spends a round");
        assert!(e.missing_counted, "counted missing once");
    }

    #[test]
    fn test_auto_mode_dispatches_the_missing_instruments_only() {
        let (book, mut rx) = SubscribeReconcile::new(true);
        let (tx, _sock) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &sis(&[1, 2, 3]), t0);
        let summary = one_sweep(&book, t0 + delta(), IN_SESSION, &[1], &[2, 3]);
        assert_eq!(summary.dispatched, 1);
        let round = rx.try_recv().expect("a round");
        assert_eq!(round.instruments, sis(&[2, 3]));
        assert!(
            round.tx.same_channel(&tx),
            "the round targets the holding socket"
        );
        assert_eq!(entry(&book, 2).rounds, 1);
    }

    #[test]
    fn test_no_round_outside_the_session_or_in_its_first_120_s() {
        let (book, mut rx) = SubscribeReconcile::new(true);
        let (tx, _sock) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &sis(&[1, 2]), t0);
        for secs in [
            8 * 3600,
            9 * 3600 + 15 * 60,
            9 * 3600 + 16 * 60 + 59,
            16 * 3600,
        ] {
            let s = one_sweep(&book, t0 + delta(), secs, &[1], &[2]);
            assert_eq!(s.dispatched, 0, "no round at secs {secs}");
        }
        assert!(rx.try_recv().is_err());
        // 09:17:00 is the first instant a round may run.
        let s = one_sweep(&book, t0 + delta(), 9 * 3600 + 17 * 60, &[1], &[2]);
        assert_eq!(s.dispatched, 1);
    }

    #[test]
    fn test_a_socket_where_nothing_ticks_is_left_to_the_redial() {
        let (book, mut rx) = SubscribeReconcile::new(true);
        let (tx_a, _a) = socket();
        let (tx_b, _b) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx_a, &sis(&[1]), t0);
        book.record_subscribed_at(&tx_b, &sis(&[2, 3]), t0);
        // Socket B: nothing ticks. Socket A ticks, so the feed is alive.
        let s = one_sweep(&book, t0 + delta(), IN_SESSION, &[1], &[2, 3]);
        assert_eq!(s.missing, 2);
        assert_eq!(
            s.dispatched, 0,
            "a silent socket is not a partial subscribe"
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_a_socket_that_went_quiet_after_ticking_is_not_delivering() {
        // Instrument 1 ticked earlier and is now quiet past its baseline
        // (Exceeded): socket A is not delivering now, so its never-ticked
        // instrument is not resent during what may be a halt.
        let (book, mut rx) = SubscribeReconcile::new(true);
        let (tx_a, _a) = socket();
        let (tx_b, _b) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx_a, &sis(&[1, 2]), t0);
        book.record_subscribed_at(&tx_b, &sis(&[3]), t0);
        let mut sweep = book.sweep_at(t0 + delta());
        sweep.note(&report(1, SilenceVerdict::Exceeded));
        sweep.note(&report(2, SilenceVerdict::NeverTicked));
        sweep.note(&report(3, SilenceVerdict::Live));
        let s = sweep.finish_at(IN_SESSION, DAY);
        assert_eq!(s.missing, 1);
        assert_eq!(s.dispatched, 0);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_a_warming_up_never_ticked_contract_still_reads_missing() {
        // Review 2026-11-06 (plan PR AL-5): the attach warmup gates only the
        // RISK-GAP-03 page. Reconcile reads the raw verdict, so a contract
        // inside its warmup that never ticked is still missing, never
        // delivering or recovered.
        let (book, _rx) = SubscribeReconcile::new(true);
        let (tx, _sock) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &sis(&[1, 2]), t0);
        let mut sweep = book.sweep_at(t0 + delta());
        sweep.note(&report(1, SilenceVerdict::Live));
        sweep.note(&SilenceReport {
            warming_up: true,
            ..report(2, SilenceVerdict::NeverTicked)
        });
        let s = sweep.finish_at(IN_SESSION, DAY);
        assert_eq!(s.missing, 1, "the warming contract is counted missing");
    }

    #[test]
    fn test_a_silent_feed_is_a_halt_not_a_partial_subscribe() {
        let (book, mut rx) = SubscribeReconcile::new(true);
        let (tx, _sock) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &sis(&[1, 2, 3]), t0);
        let mut sweep = book.sweep_at(t0 + delta());
        // Everything that ever ticked is quiet now; the rest never ticked.
        sweep.note(&report(1, SilenceVerdict::Exceeded));
        sweep.note(&report(2, SilenceVerdict::NeverTicked));
        sweep.note(&report(3, SilenceVerdict::NeverTicked));
        let s = sweep.finish_at(IN_SESSION, DAY);
        assert_eq!(s.missing, 2);
        assert_eq!(s.dispatched, 0);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_one_round_in_flight_per_socket() {
        let (book, mut rx) = SubscribeReconcile::new(true);
        let (tx, _sock) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &sis(&[1, 2]), t0);
        assert_eq!(
            one_sweep(&book, t0 + delta(), IN_SESSION, &[1], &[2]).dispatched,
            1
        );
        // The round is still running ten minutes later: nothing new is planned.
        let late = t0 + delta() + Duration::from_secs(600);
        assert_eq!(one_sweep(&book, late, IN_SESSION, &[1], &[2]).dispatched, 0);
        assert_eq!(
            entry(&book, 2).rounds,
            1,
            "no round charged while one is in flight"
        );
        // It finishes; the spacing runs from the FINISH, not from the plan.
        let done = late + Duration::from_secs(1);
        assert_eq!(finish_queued(&book, &mut rx, done), 1);
        assert_eq!(
            one_sweep(
                &book,
                done + Duration::from_secs(119),
                IN_SESSION,
                &[1],
                &[2]
            )
            .dispatched,
            0
        );
        assert_eq!(
            one_sweep(&book, done + delta(), IN_SESSION, &[1], &[2]).dispatched,
            1
        );
    }

    #[test]
    fn test_rounds_are_capped_and_then_exhausted_once() {
        let (book, mut rx) = SubscribeReconcile::new(true);
        let (tx, _sock) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &sis(&[1, 2]), t0);
        let mut at = t0 + delta();
        let mut rounds = 0;
        let mut exhausted = 0;
        // 30 s sweeps for 20 minutes; each round finishes right away.
        for _ in 0..40 {
            let s = one_sweep(&book, at, IN_SESSION, &[1], &[2]);
            exhausted += s.newly_exhausted;
            rounds += finish_queued(&book, &mut rx, at);
            at += Duration::from_secs(30);
        }
        assert_eq!(rounds, usize::from(RECONCILE_MAX_ROUNDS));
        assert_eq!(exhausted, 1, "reported exhausted in exactly one sweep");
        let e = entry(&book, 2);
        assert_eq!(e.rounds, RECONCILE_MAX_ROUNDS);
        assert!(e.exhausted_reported);
    }

    // Regression: 2026-10-30 — `note` pushed every exhausted instrument into
    // an empty Vec under the book's lock, allocating on the drain.
    #[test]
    fn test_regression_exhausted_past_the_named_cap_counts_all_and_never_grows() {
        let (book, mut rx) = SubscribeReconcile::new(true);
        let (tx, _sock) = socket();
        let t0 = Instant::now();
        let missing: Vec<u64> = (2..2 + 3 * NAMED_IN_LOG as u64).collect();
        let mut all = vec![1];
        all.extend_from_slice(&missing);
        book.record_subscribed_at(&tx, &sis(&all), t0);
        let capacity = book.lock().exhausted_named.capacity();
        assert!(capacity >= NAMED_IN_LOG);
        let mut at = t0 + delta();
        let mut exhausted = 0;
        for _ in 0..40 {
            let s = one_sweep(&book, at, IN_SESSION, &[1], &missing);
            exhausted += s.newly_exhausted;
            finish_queued(&book, &mut rx, at);
            at += Duration::from_secs(30);
        }
        assert_eq!(
            exhausted as usize,
            missing.len(),
            "every exhausted instrument is counted"
        );
        let inner = book.lock();
        assert!(inner.exhausted_named.len() <= NAMED_IN_LOG);
        assert_eq!(
            inner.exhausted_named.capacity(),
            capacity,
            "the named list is never grown"
        );
    }

    #[test]
    fn test_exhausted_waits_for_the_last_round_to_settle() {
        let (book, mut rx) = SubscribeReconcile::new(true);
        let (tx, _sock) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &sis(&[1, 2]), t0);
        let mut at = t0 + delta();
        let mut last_finish = at;
        for _ in 0..RECONCILE_MAX_ROUNDS {
            one_sweep(&book, at, IN_SESSION, &[1], &[2]);
            finish_queued(&book, &mut rx, at);
            last_finish = at;
            at += delta();
        }
        one_sweep(
            &book,
            last_finish + Duration::from_secs(30),
            IN_SESSION,
            &[1],
            &[2],
        );
        assert!(
            !entry(&book, 2).exhausted_reported,
            "too soon after the last round"
        );
        one_sweep(&book, last_finish + delta(), IN_SESSION, &[1], &[2]);
        assert!(entry(&book, 2).exhausted_reported);
    }

    #[test]
    fn test_a_new_ist_day_gives_rounds_back() {
        let (book, mut rx) = SubscribeReconcile::new(true);
        let (tx, _sock) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &sis(&[1, 2]), t0);
        let mut at = t0 + delta();
        let mut spent = 0;
        // Exactly the three rounds, so the rollover sweep below is the first
        // one where the last round has settled: it WOULD report instrument 2
        // exhausted, and only the rollover stops it.
        for _ in 0..RECONCILE_MAX_ROUNDS {
            one_sweep(&book, at, IN_SESSION, &[1], &[2]);
            spent += finish_queued(&book, &mut rx, at);
            at += delta();
        }
        assert_eq!(
            spent,
            usize::from(RECONCILE_MAX_ROUNDS),
            "yesterday is spent"
        );
        // The rollover runs in finish, after this sweep gathered its
        // candidates against yesterday's spent rounds, so it sends nothing.
        let s = one_sweep_on(&book, at, IN_SESSION, DAY + 1, &[1], &[2]);
        assert_eq!(s.dispatched, 0);
        assert_eq!(
            s.newly_exhausted, 0,
            "yesterday's exhaustion is not re-reported"
        );
        let e = entry(&book, 2);
        assert_eq!(e.rounds, 0);
        assert!(!e.missing_counted, "counted again on the new day");
        let s = one_sweep_on(
            &book,
            at + Duration::from_secs(30),
            IN_SESSION,
            DAY + 1,
            &[1],
            &[2],
        );
        assert_eq!(s.dispatched, 1, "the new day resends");
    }

    #[test]
    fn test_a_full_executor_queue_spends_no_round() {
        let (book, _rx) = SubscribeReconcile::new(true);
        // Fill the executor queue without draining it.
        for _ in 0..RECONCILE_EXECUTOR_QUEUE {
            let (tx, _s) = socket();
            book.rounds_tx
                .try_send(PlannedRound {
                    tx,
                    socket: 0,
                    instruments: Vec::new(),
                    reservation: 0,
                    ist_day: DAY,
                })
                .expect("room");
        }
        let (tx, _sock) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &sis(&[1, 2]), t0);
        let s = one_sweep(&book, t0 + delta(), IN_SESSION, &[1], &[2]);
        assert_eq!(s.dispatched, 0);
        let inner = book.lock();
        assert_eq!(inner.expected[&(2, ExchangeSegment::NseFno)].rounds, 0);
        assert!(
            inner.sockets[0].last_round_at.is_none(),
            "spacing not consumed"
        );
        assert!(inner.sockets[0].in_flight.is_none());
    }

    #[test]
    fn test_record_subscribed_keeps_the_first_socket_and_dedups() {
        let (book, _rx) = SubscribeReconcile::new(false);
        let (tx_a, _a) = socket();
        let (tx_b, _b) = socket();
        book.record_subscribed(&tx_a, &sis(&[1, 1]));
        book.record_subscribed(&tx_b, &sis(&[1, 2]));
        assert_eq!(book.expected_len(), 2);
        assert_eq!(entry(&book, 1).socket, 0);
        assert_eq!(entry(&book, 2).socket, 1);
        let inner = book.lock();
        assert_eq!(inner.sockets.len(), 2);
        assert!(
            inner.sockets[0].candidates.capacity() >= RECONCILE_MAX_PER_SOCKET_PER_ROUND,
            "candidates are pre-sized so note never grows them"
        );
    }

    #[test]
    fn test_expected_len_counts_one_id_in_two_segments_twice() {
        // I-P1-11: the key is (security_id, segment), never the id alone.
        let (book, _rx) = SubscribeReconcile::new(false);
        let (tx, _s) = socket();
        let eq = SubscribeInstrument {
            security_id: 27,
            segment: ExchangeSegment::NseEquity,
        };
        let idx = SubscribeInstrument {
            security_id: 27,
            segment: ExchangeSegment::IdxI,
        };
        book.record_subscribed(&tx, &[eq, idx]);
        assert_eq!(book.expected_len(), 2);
    }

    #[test]
    fn test_forget_on_preserves_rebound_owner_and_daily_rounds() {
        let (book, _rounds) = SubscribeReconcile::new(false);
        let (first, receiver) = socket();
        let (replacement, _replacement_receiver) = socket();
        let started = Instant::now();
        book.record_subscribed_at(&first, &[si(1)], started);
        book.lock()
            .expected
            .get_mut(&key_of(&si(1)))
            .expect("entry")
            .rounds = 2;
        drop(receiver);
        let reoffered = started + delta();
        book.record_subscribed_at(&replacement, &[si(1)], reoffered);
        let current = entry(&book, 1);
        assert_eq!(current.socket, 1);
        assert_eq!(current.recorded_at, reoffered);
        assert_eq!(
            current.rounds, 2,
            "rebind cannot reset the daily resend cap"
        );
        book.forget_on(&first, &[si(1)]);
        assert_eq!(
            book.expected_len(),
            1,
            "the old ACK cannot erase the new owner"
        );
        book.forget_on(&replacement, &[si(1)]);
        assert_eq!(book.expected_len(), 0);
    }

    #[test]
    fn test_forget_on_backup_refusal_preserves_primary_timestamp_and_expected_entry() {
        let (book, _rounds) = SubscribeReconcile::new(false);
        let (primary, _primary_receiver) = socket();
        let (backup, _backup_receiver) = socket();
        let started = Instant::now();
        book.record_subscribed_at(&primary, &[si(1)], started);
        book.record_subscribed_at(&backup, &[si(1), si(2)], started + delta());
        book.forget_on(&backup, &[si(1), si(2), si(99)]);
        assert_eq!(book.expected_len(), 1);
        let current = entry(&book, 1);
        assert_eq!(current.socket, 0);
        assert_eq!(current.recorded_at, started);
    }

    #[test]
    fn test_forget_and_note_ignore_unknown_reports() {
        let (book, _rx) = SubscribeReconcile::new(true);
        let (tx, _s) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &sis(&[1, 2]), t0);
        book.forget(&sis(&[2]));
        // id 99 was never recorded: its report is ignored.
        let s = one_sweep(&book, t0 + delta(), IN_SESSION, &[1], &[2, 99]);
        assert_eq!(s.missing, 0);
    }

    #[test]
    fn test_the_socket_cap_refuses_new_sockets() {
        let (book, _rx) = SubscribeReconcile::new(false);
        let mut keep = Vec::new();
        for id in 0..(RECONCILE_MAX_SOCKETS as u64 + 3) {
            let (tx, rx) = socket();
            book.record_subscribed(&tx, &[si(id)]);
            keep.push((tx, rx));
        }
        assert_eq!(book.expected_len(), RECONCILE_MAX_SOCKETS);
        assert_eq!(book.lock().sockets.len(), RECONCILE_MAX_SOCKETS);
    }

    #[test]
    fn test_one_round_is_capped_per_socket() {
        let (book, mut rx) = SubscribeReconcile::new(true);
        let (tx, _s) = socket();
        let t0 = Instant::now();
        let n = RECONCILE_MAX_PER_SOCKET_PER_ROUND as u64 + 50;
        let all: Vec<SubscribeInstrument> = (1..=n + 1).map(si).collect();
        book.record_subscribed_at(&tx, &all, t0);
        let missing: Vec<u64> = (2..=n + 1).collect();
        one_sweep(&book, t0 + delta(), IN_SESSION, &[1], &missing);
        let round = rx.try_recv().expect("a round");
        assert_eq!(round.instruments.len(), RECONCILE_MAX_PER_SOCKET_PER_ROUND);
    }

    #[test]
    fn test_named_instruments_truncates() {
        assert_eq!(
            NamedInstruments(&sis(&[1, 2])).to_string(),
            "NSE_FNO:1,NSE_FNO:2"
        );
        let many: Vec<SubscribeInstrument> = (0..25).map(si).collect();
        let text = NamedInstruments(&many).to_string();
        assert!(text.ends_with(",+5 more"), "{text}");
        assert_eq!(NamedInstruments(&[]).to_string(), "");
    }

    #[test]
    fn test_without_removes_on_the_composite_key() {
        assert_eq!(without(sis(&[1, 2, 3]), &sis(&[2])), sis(&[1, 3]));
        assert_eq!(without(sis(&[1, 2]), &[]), sis(&[1, 2]));
        let other_segment = SubscribeInstrument {
            security_id: 2,
            segment: ExchangeSegment::IdxI,
        };
        assert_eq!(
            without(sis(&[1, 2]), &[other_segment]),
            sis(&[1, 2]),
            "same id in another segment is a different instrument"
        );
    }

    #[test]
    fn test_finish_now_reads_the_wall_clock_without_panicking() {
        // Whatever the wall clock says, finish_now must classify the pass and
        // return; shadow mode never dispatches.
        let (book, _rx) = SubscribeReconcile::new(false);
        let (tx, _s) = socket();
        book.record_subscribed(&tx, &[si(1)]);
        let mut sweep = book.sweep();
        sweep.note(&report(1, SilenceVerdict::Live));
        let summary = sweep.finish_now();
        assert_eq!(summary.dispatched, 0);
    }

    #[test]
    fn test_ist_day_now_is_a_plausible_day() {
        // 2026-01-01 is day 20,454 since the Unix epoch.
        assert!(ist_day_now() > 20_454);
    }

    // ---- executor ---------------------------------------------------------

    type Seen = Vec<(Vec<SubscribeInstrument>, Vec<SubscribeInstrument>)>;

    /// A fake connection task that answers from a script (`None` = drop the
    /// ack unanswered) and records every command it got.
    fn fake_socket(
        script: Vec<Option<ResubscribeOutcome>>,
    ) -> (
        mpsc::Sender<LiveSubscriptionCommand>,
        tokio::task::JoinHandle<Seen>,
    ) {
        let (tx, mut rx) = mpsc::channel(8);
        let handle = tokio::spawn(async move {
            let mut seen = Vec::new();
            let mut script = script.into_iter();
            while let Some(command) = rx.recv().await {
                let LiveSubscriptionCommand::Resubscribe {
                    unsubscribe,
                    subscribe,
                    ack,
                } = command
                else {
                    panic!("the reconciler must only send Resubscribe");
                };
                seen.push((unsubscribe, subscribe));
                match script.next().flatten() {
                    Some(outcome) => {
                        if let Some(ack) = ack {
                            let _ = ack.send(outcome);
                        }
                    }
                    None => drop(ack),
                }
            }
            seen
        });
        (tx, handle)
    }

    /// A fake connection task that behaves like the real guard: an unsubscribe
    /// naming any instrument it does not hold is refused whole; a subscribe of
    /// a held one is a (logged) duplicate, counted here.
    fn guard_socket(
        held: Vec<SubscribeInstrument>,
    ) -> (
        mpsc::Sender<LiveSubscriptionCommand>,
        tokio::task::JoinHandle<(Vec<SubscribeInstrument>, usize, usize)>,
    ) {
        capped_guard_socket(held, usize::MAX, 0)
    }

    /// [`guard_socket`] with the real per-socket cap check: a command whose
    /// result would hold more than `cap` instruments is refused whole. The
    /// first `unanswered` commands are applied (or refused) but never
    /// answered, as when the ack is lost.
    fn capped_guard_socket(
        held: Vec<SubscribeInstrument>,
        cap: usize,
        unanswered: usize,
    ) -> (
        mpsc::Sender<LiveSubscriptionCommand>,
        tokio::task::JoinHandle<(Vec<SubscribeInstrument>, usize, usize)>,
    ) {
        let (tx, mut rx) = mpsc::channel(8);
        let handle = tokio::spawn(async move {
            let mut held = held;
            let mut commands = 0usize;
            let mut duplicates = 0usize;
            while let Some(command) = rx.recv().await {
                let LiveSubscriptionCommand::Resubscribe {
                    unsubscribe,
                    subscribe,
                    ack,
                } = command
                else {
                    panic!("the reconciler must only send Resubscribe");
                };
                commands += 1;
                let outcome = if unsubscribe.iter().any(|i| !held.contains(i)) {
                    ResubscribeOutcome::Refused {
                        reason: ResubscribeOutcome::REASON_NOT_HELD,
                    }
                } else if held.len() - unsubscribe.len()
                    + subscribe.iter().filter(|i| !held.contains(i)).count()
                    > cap
                {
                    ResubscribeOutcome::Refused {
                        reason: ResubscribeOutcome::REASON_OVER_CAP,
                    }
                } else {
                    held.retain(|i| !unsubscribe.contains(i));
                    for i in subscribe {
                        if held.contains(&i) {
                            duplicates += 1;
                        } else {
                            held.push(i);
                        }
                    }
                    ResubscribeOutcome::Applied
                };
                if commands > unanswered
                    && let Some(ack) = ack
                {
                    let _ = ack.send(outcome);
                }
            }
            held.sort_by_key(|i| i.security_id);
            (held, commands, duplicates)
        });
        (tx, handle)
    }

    /// Runs one round through the executor with a recording `finished`.
    async fn run_round(
        tx: mpsc::Sender<LiveSubscriptionCommand>,
        ids: &[u64],
    ) -> (RoundOutcome, Vec<SubscribeInstrument>) {
        let mut refunded = Vec::new();
        let outcome = execute_round_on(&tx, sis(ids), |refund| refunded = refund.to_vec()).await;
        (outcome, refunded)
    }

    #[tokio::test]
    async fn test_execute_round_unsubscribes_then_subscribes_on_the_same_socket() {
        let (tx, handle) = fake_socket(vec![
            Some(ResubscribeOutcome::Applied),
            Some(ResubscribeOutcome::Applied),
        ]);
        let (outcome, refunded) = run_round(tx, &[5, 6]).await;
        assert_eq!(
            outcome,
            RoundOutcome::Resubscribed {
                resent: 2,
                not_held: 0,
                unplaced: 0
            }
        );
        assert!(refunded.is_empty());
        let seen = handle.await.expect("fake socket");
        assert_eq!(seen, vec![(sis(&[5, 6]), vec![]), (vec![], sis(&[5, 6]))]);
    }

    #[tokio::test]
    async fn test_execute_round_splits_a_not_held_refusal_and_subscribes_the_unheld_one() {
        // The socket holds 1, 2, 4 but not 3 (lost on an earlier round): the
        // whole-batch refusal must not poison 1, 2 and 4.
        let (tx, handle) = guard_socket(sis(&[1, 2, 4]));
        let (outcome, refunded) = run_round(tx, &[1, 2, 3, 4]).await;
        assert_eq!(
            outcome,
            RoundOutcome::Resubscribed {
                resent: 4,
                not_held: 1,
                unplaced: 0
            }
        );
        assert!(refunded.is_empty(), "every instrument was resent");
        let (held, commands, duplicates) = handle.await.expect("guard socket");
        assert_eq!(held, sis(&[1, 2, 3, 4]), "all four are on the socket again");
        assert_eq!(duplicates, 0, "never re-offers a held instrument");
        assert!(commands <= 8, "the split is bounded: {commands} commands");
    }

    // Regression: 2026-10-30 (final review) — one not-held instrument in a
    // round on a full socket pushed the joint subscribe over the cap, and the
    // held instruments the round had just taken off were lost with it.
    #[tokio::test]
    async fn test_regression_a_not_held_instrument_never_costs_the_held_ones_their_slots() {
        tokio::time::pause();
        // A full socket of 5: holds 1, 2, 4, 9, 10 and not 3.
        let (tx, handle) = capped_guard_socket(sis(&[1, 2, 4, 9, 10]), 5, 0);
        let (outcome, refunded) = run_round(tx, &[1, 2, 3, 4]).await;
        assert_eq!(
            outcome,
            RoundOutcome::Resubscribed {
                resent: 3,
                not_held: 1,
                unplaced: 1
            }
        );
        assert!(refunded.is_empty(), "3 was tried, so its round is spent");
        let (held, _, duplicates) = handle.await.expect("guard socket");
        assert_eq!(
            held,
            sis(&[1, 2, 4, 9, 10]),
            "every held instrument is back; 3 found no room"
        );
        assert_eq!(duplicates, 0);
    }

    #[tokio::test]
    async fn test_execute_round_with_room_places_the_not_held_one() {
        let (tx, handle) = capped_guard_socket(sis(&[1, 2]), 3, 0);
        let (outcome, _) = run_round(tx, &[1, 2, 3]).await;
        assert_eq!(
            outcome,
            RoundOutcome::Resubscribed {
                resent: 3,
                not_held: 1,
                unplaced: 0
            }
        );
        let (held, _, _) = handle.await.expect("guard socket");
        assert_eq!(held, sis(&[1, 2, 3]));
    }

    // Regression: 2026-10-30 (final review, second pass) — an unsubscribe the
    // socket refused as not-held but never answered put a not-held instrument
    // in the `unknown` group, which then pushed phase one over the cap, and
    // held instruments that never left were reported lost.
    #[tokio::test]
    async fn test_regression_an_unanswered_unsubscribe_never_reports_held_ones_lost() {
        tokio::time::pause();
        // Full socket of 5: holds 1, 2, 4, 9, 10, not 3. The first command
        // (the unsubscribe of 1..4) is refused as not-held and never answered.
        let (tx, handle) = capped_guard_socket(sis(&[1, 2, 4, 9, 10]), 5, 1);
        let (outcome, refunded) = run_round(tx, &[1, 2, 3, 4]).await;
        assert_eq!(
            outcome,
            RoundOutcome::Unknown(sis(&[1, 2, 3, 4])),
            "may or may not be on the socket; never lost"
        );
        assert!(refunded.is_empty());
        let (held, _, _) = handle.await.expect("guard socket");
        assert_eq!(held, sis(&[1, 2, 4, 9, 10]), "nothing left the socket");
    }

    /// Counts `warn!` events on the thread it is the default subscriber of.
    struct WarnEvents(Arc<std::sync::atomic::AtomicUsize>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for WarnEvents {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() == tracing::Level::WARN {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    // Regression: 2026-10-30 (final review, third pass) — when phase one lost
    // instruments and phase two left others uncertain, the round returned
    // Lost with the lost ones only and nothing named the uncertain ones.
    #[tokio::test]
    async fn test_regression_uncertain_ones_are_named_when_others_are_lost() {
        use tracing_subscriber::layer::SubscriberExt;
        tokio::time::pause();
        let warns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        /// Keeps a second, inert `tracing` dispatcher registered for the whole test
        /// process. While a scoped subscriber is the only registered dispatcher,
        /// tracing-core caches a callsite as `never` when a thread with no
        /// subscriber reaches it first, and the scoped subscriber then misses the
        /// line. Call before installing the scoped subscriber.
        fn keep_callsite_interest_shared() {
            static ANCHOR: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();
            ANCHOR.get_or_init(|| {
                tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default())
            });
        }

        keep_callsite_interest_shared();
        let _log = tracing::subscriber::set_default(
            tracing_subscriber::registry().with(WarnEvents(Arc::clone(&warns))),
        );
        let refused = |reason| Some(ResubscribeOutcome::Refused { reason });
        let (tx, handle) = fake_socket(vec![
            // The unsubscribe of [5, 6] is refused whole, so it is split.
            refused(ResubscribeOutcome::REASON_NOT_HELD),
            // One half leaves the socket; the other is never answered.
            Some(ResubscribeOutcome::Applied),
            None,
            // Phase one (the one that left) finds the slot taken, three times.
            refused(ResubscribeOutcome::REASON_OVER_CAP),
            refused(ResubscribeOutcome::REASON_OVER_CAP),
            refused(ResubscribeOutcome::REASON_OVER_CAP),
            // Phase two (the unanswered one) is refused too.
            refused(ResubscribeOutcome::REASON_OVER_CAP),
            refused(ResubscribeOutcome::REASON_OVER_CAP),
            refused(ResubscribeOutcome::REASON_OVER_CAP),
        ]);
        let (outcome, refunded) = run_round(tx, &[5, 6]).await;
        let RoundOutcome::Lost(lost) = outcome else {
            panic!("expected Lost, got {outcome:?}");
        };
        assert_eq!(lost.len(), 1, "only the one that left is lost");
        assert!(refunded.is_empty());
        assert_eq!(
            warns.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the uncertain one is named in its own warn"
        );
        assert_eq!(handle.await.expect("fake").len(), 9);
    }

    // Regression: 2026-10-30 (final review, third pass) — `Warming` never
    // looks at how long an instrument has been quiet, so a thin contract that
    // ticked at 09:05 counted as "delivering" hours later and let a round
    // through during a halt.
    #[test]
    fn test_regression_a_stale_warming_instrument_is_not_delivering() {
        let judge = |silent_millis: u64| {
            let (book, _rx) = SubscribeReconcile::new(true);
            let (tx, _sock) = socket();
            let t0 = Instant::now();
            book.record_subscribed_at(&tx, &sis(&[1, 2]), t0);
            let mut sweep = book.sweep_at(t0 + delta());
            sweep.note(&SilenceReport {
                silent_millis,
                expected_millis: DEFAULT_WARMING_EXPECTED_MILLIS,
                ..report(1, SilenceVerdict::Warming)
            });
            sweep.note(&report(2, SilenceVerdict::NeverTicked));
            sweep.finish_at(IN_SESSION, DAY).dispatched
        };
        assert_eq!(judge(4 * 3_600_000), 0, "quiet for 4 h: not delivering");
        assert_eq!(judge(10_000), 1, "ticked 10 s ago: delivering");
    }

    /// The detector's silence floor, the least `expected_millis` can be.
    const DEFAULT_WARMING_EXPECTED_MILLIS: u64 = 30_000;

    #[tokio::test]
    async fn test_execute_round_split_stops_at_the_command_cap_and_refunds_the_rest() {
        // Nothing held and 1,000 instruments: the bisection would need ~2,000
        // commands to find every not-held single, so it stops at the cap.
        let ids: Vec<u64> = (1..=RECONCILE_MAX_PER_SOCKET_PER_ROUND as u64).collect();
        let (tx, handle) = guard_socket(Vec::new());
        let (outcome, refunded) = run_round(tx, &ids).await;
        let RoundOutcome::Resubscribed {
            resent,
            not_held,
            unplaced,
        } = outcome
        else {
            panic!("expected Resubscribed, got {outcome:?}");
        };
        assert_eq!(unplaced, 0);
        assert_eq!(
            resent, not_held,
            "only the found not-held singles are placed"
        );
        assert!(not_held > 0 && not_held < ids.len());
        assert_eq!(
            not_held + refunded.len(),
            ids.len(),
            "every instrument is either placed or gets its round back"
        );
        let (held, commands, duplicates) = handle.await.expect("guard socket");
        assert_eq!(held.len(), not_held);
        assert_eq!(duplicates, 0);
        assert_eq!(
            commands,
            RECONCILE_MAX_UNSUBSCRIBE_COMMANDS + 1,
            "the unsubscribe split is capped, then one subscribe"
        );
    }

    #[tokio::test]
    async fn test_execute_round_socket_closed_between_the_halves_reports_lost() {
        tokio::time::pause();
        // The unsubscribe applies, then the socket's task goes away.
        let (tx, mut rx) = mpsc::channel::<LiveSubscriptionCommand>(8);
        let socket = tokio::spawn(async move {
            if let Some(LiveSubscriptionCommand::Resubscribe { ack: Some(ack), .. }) =
                rx.recv().await
            {
                let _ = ack.send(ResubscribeOutcome::Applied);
            }
        });
        let (outcome, refunded) = run_round(tx, &[5, 6]).await;
        socket.await.expect("socket");
        assert_eq!(outcome, RoundOutcome::Lost(sis(&[5, 6])));
        assert!(refunded.is_empty());
    }

    #[tokio::test]
    async fn test_execute_round_split_with_nothing_held_subscribes_everything() {
        let (tx, handle) = guard_socket(Vec::new());
        let (outcome, _) = run_round(tx, &[1, 2, 3]).await;
        assert_eq!(
            outcome,
            RoundOutcome::Resubscribed {
                resent: 3,
                not_held: 3,
                unplaced: 0
            }
        );
        let (held, _, duplicates) = handle.await.expect("guard socket");
        assert_eq!(held, sis(&[1, 2, 3]));
        assert_eq!(duplicates, 0);
    }

    #[tokio::test]
    async fn test_execute_round_stopped_unsubscribe_resends_only_what_left() {
        let (tx, handle) = fake_socket(vec![
            Some(ResubscribeOutcome::Stopped {
                not_unsubscribed: sis(&[6]),
                not_subscribed: vec![],
            }),
            Some(ResubscribeOutcome::Applied),
        ]);
        let (outcome, refunded) = run_round(tx, &[5, 6]).await;
        assert_eq!(
            outcome,
            RoundOutcome::Resubscribed {
                resent: 1,
                not_held: 0,
                unplaced: 0
            }
        );
        assert_eq!(
            refunded,
            sis(&[6]),
            "the one never resent gets its round back"
        );
        let seen = handle.await.expect("fake");
        assert_eq!(
            seen[1],
            (vec![], sis(&[5])),
            "the still-held one is not re-offered"
        );
    }

    #[tokio::test]
    async fn test_execute_round_unreachable_socket_is_send_failed_and_refunded() {
        let (tx, rx) = socket();
        drop(rx);
        let (outcome, refunded) = run_round(tx, &[5, 6]).await;
        assert_eq!(outcome, RoundOutcome::SendFailed);
        assert_eq!(refunded, sis(&[5, 6]));
    }

    #[tokio::test]
    async fn test_execute_round_after_an_805_changes_nothing() {
        let (tx, handle) = fake_socket(vec![Some(ResubscribeOutcome::Refused {
            reason: ResubscribeOutcome::REASON_HALTED_805,
        })]);
        let (outcome, refunded) = run_round(tx, &[5]).await;
        assert_eq!(
            outcome,
            RoundOutcome::Refused {
                reason: ResubscribeOutcome::REASON_HALTED_805
            }
        );
        assert_eq!(refunded, sis(&[5]));
        assert_eq!(
            handle.await.expect("fake").len(),
            1,
            "nothing sent after it"
        );
    }

    #[tokio::test]
    async fn test_execute_round_retries_the_part_not_subscribed() {
        let (tx, handle) = fake_socket(vec![
            Some(ResubscribeOutcome::ReplayPending),
            Some(ResubscribeOutcome::Stopped {
                not_unsubscribed: vec![],
                not_subscribed: sis(&[6]),
            }),
            Some(ResubscribeOutcome::Applied),
        ]);
        tokio::time::pause();
        let (outcome, _) = run_round(tx, &[5, 6]).await;
        assert_eq!(
            outcome,
            RoundOutcome::Resubscribed {
                resent: 2,
                not_held: 0,
                unplaced: 0
            }
        );
        let seen = handle.await.expect("fake");
        assert_eq!(seen.len(), 3);
        assert_eq!(
            seen[2],
            (vec![], sis(&[6])),
            "only the unsent part is retried"
        );
    }

    #[tokio::test]
    async fn test_execute_round_unanswered_unsubscribe_still_resubscribes() {
        // The unsubscribe's answer never came back: treated as done, so the
        // subscribe half still runs, once.
        let (tx, handle) = fake_socket(vec![None, Some(ResubscribeOutcome::Applied)]);
        tokio::time::pause();
        let (outcome, _) = run_round(tx, &[5]).await;
        assert_eq!(
            outcome,
            RoundOutcome::Resubscribed {
                resent: 1,
                not_held: 0,
                unplaced: 0
            }
        );
        assert_eq!(handle.await.expect("fake").len(), 2);
    }

    #[tokio::test]
    async fn test_execute_round_unanswered_subscribe_is_unknown_not_lost() {
        let (tx, handle) = fake_socket(vec![Some(ResubscribeOutcome::Applied), None]);
        tokio::time::pause();
        let (outcome, _) = run_round(tx, &[5, 6]).await;
        assert_eq!(outcome, RoundOutcome::Unknown(sis(&[5, 6])));
        assert_eq!(
            handle.await.expect("fake").len(),
            2,
            "an unanswered subscribe is not resent blindly"
        );
    }

    #[tokio::test]
    async fn test_execute_round_reports_lost_after_every_attempt_fails() {
        let mut script = vec![Some(ResubscribeOutcome::Applied)];
        for _ in 0..RECONCILE_SUBSCRIBE_ATTEMPTS {
            script.push(Some(ResubscribeOutcome::Refused {
                reason: ResubscribeOutcome::REASON_OVER_CAP,
            }));
        }
        let (tx, handle) = fake_socket(script);
        tokio::time::pause();
        let (outcome, _) = run_round(tx, &[5, 6]).await;
        assert_eq!(outcome, RoundOutcome::Lost(sis(&[5, 6])));
        assert_eq!(
            handle.await.expect("fake").len(),
            1 + RECONCILE_SUBSCRIBE_ATTEMPTS as usize,
            "bounded: one unsubscribe and at most the attempt cap of subscribes"
        );
    }

    #[tokio::test]
    async fn test_execute_round_reports_back_to_the_book() {
        let (book, mut rx) = SubscribeReconcile::new(true);
        let (tx, handle) = fake_socket(vec![
            Some(ResubscribeOutcome::Stopped {
                not_unsubscribed: sis(&[3]),
                not_subscribed: vec![],
            }),
            Some(ResubscribeOutcome::Applied),
        ]);
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &sis(&[1, 2, 3]), t0);
        assert_eq!(
            one_sweep(&book, t0 + delta(), IN_SESSION, &[1], &[2, 3]).dispatched,
            1
        );
        let round = rx.try_recv().expect("a round");
        assert!(book.lock().sockets[0].in_flight.is_some());
        let outcome = execute_round_with_clock(&book, round, &mut || (IN_SESSION, DAY)).await;
        assert_eq!(
            outcome,
            RoundOutcome::Resubscribed {
                resent: 1,
                not_held: 0,
                unplaced: 0
            }
        );
        {
            let inner = book.lock();
            assert!(
                inner.sockets[0].in_flight.is_none(),
                "the in-flight flag is cleared"
            );
            assert!(inner.sockets[0].last_round_at.is_some());
            assert_eq!(inner.expected[&(2, ExchangeSegment::NseFno)].rounds, 1);
            assert_eq!(
                inner.expected[&(3, ExchangeSegment::NseFno)].rounds,
                0,
                "never resent, so refunded"
            );
        }
        // The book keeps a clone of the socket's sender, so the fake socket
        // ends only once both are gone.
        drop(tx);
        drop(book);
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("the fake socket ends once every sender is dropped")
            .expect("fake");
    }

    #[tokio::test]
    async fn test_run_reconcile_executor_runs_rounds_until_the_book_is_dropped() {
        let (book, rounds_rx) = SubscribeReconcile::new(true);
        let (tx, handle) = fake_socket(vec![
            Some(ResubscribeOutcome::Applied),
            Some(ResubscribeOutcome::Applied),
        ]);
        let started = Instant::now();
        book.record_subscribed_at(&tx, &sis(&[1, 7]), started);
        assert_eq!(
            one_sweep(&book, started + delta(), IN_SESSION, &[1], &[7]).dispatched,
            1
        );
        let executor = tokio::spawn(run_executor_with_clock(
            Arc::downgrade(&book),
            rounds_rx,
            || (IN_SESSION, DAY),
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if book.lock().sockets[0].in_flight.is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the valid round finishes while the book lives");
        drop(tx);
        drop(book);
        let seen = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("the socket ends after its book is dropped")
            .expect("fake");
        assert_eq!(seen, vec![(sis(&[7]), vec![]), (vec![], sis(&[7]))]);
        tokio::time::timeout(Duration::from_secs(5), executor)
            .await
            .expect("the executor ends when the book is dropped")
            .expect("executor task");
    }

    #[tokio::test]
    async fn test_executor_skips_a_round_queued_after_the_book_is_gone() {
        let (book, rounds_rx) = SubscribeReconcile::new(true);
        let (tx, handle) = fake_socket(vec![Some(ResubscribeOutcome::Applied)]);
        book.rounds_tx
            .try_send(PlannedRound {
                tx,
                socket: 0,
                instruments: sis(&[7]),
                reservation: 0,
                ist_day: DAY,
            })
            .expect("room");
        drop(book);
        run_reconcile_executor(std::sync::Weak::new(), rounds_rx).await;
        assert!(
            handle.await.expect("fake").is_empty(),
            "nothing sent for a gone lane"
        );
    }

    #[tokio::test]
    async fn test_queued_round_for_a_forgotten_offer_sends_no_unsubscribe() {
        let (book, mut rounds) = SubscribeReconcile::new(true);
        let (tx, mut receiver) = socket();
        let started = Instant::now();
        book.record_subscribed_at(&tx, &sis(&[1, 2]), started);
        assert_eq!(
            one_sweep(&book, started + delta(), IN_SESSION, &[1], &[2]).dispatched,
            1
        );
        let round = rounds.try_recv().expect("reserved round");
        book.forget_on(&tx, &[si(2)]);
        assert_eq!(
            execute_round_with_clock(&book, round, &mut || (IN_SESSION, DAY)).await,
            RoundOutcome::Canceled
        );
        assert!(
            receiver.try_recv().is_err(),
            "forgotten work cannot reach the socket"
        );
        assert_eq!(book.expected_len(), 1);
        assert!(book.lock().sockets[0].in_flight.is_none());
    }

    #[tokio::test]
    async fn test_first_packet_seen_during_queue_wait_cancels_unneeded_round() {
        let (book, mut rounds) = SubscribeReconcile::new(true);
        let (tx, mut receiver) = socket();
        let started = Instant::now();
        book.record_subscribed_at(&tx, &sis(&[1, 2]), started);
        one_sweep(&book, started + delta(), IN_SESSION, &[1], &[2]);
        let round = rounds.try_recv().expect("reserved round");
        one_sweep(&book, started + delta() * 2, IN_SESSION, &[1, 2], &[]);
        assert_eq!(
            execute_round_with_clock(&book, round, &mut || (IN_SESSION, DAY)).await,
            RoundOutcome::Canceled
        );
        assert!(receiver.try_recv().is_err());
        assert_eq!(entry(&book, 2).rounds, 0);
    }

    #[tokio::test]
    async fn test_closed_window_refunds_only_the_current_reservation_once() {
        let (book, mut rounds) = SubscribeReconcile::new(true);
        let (tx, mut receiver) = socket();
        let started = Instant::now();
        book.record_subscribed_at(&tx, &sis(&[1, 2]), started);
        one_sweep(&book, started + delta(), IN_SESSION, &[1], &[2]);
        let round = rounds.try_recv().expect("first reservation");
        let receipt = PlannedRound {
            tx: round.tx.clone(),
            socket: round.socket,
            instruments: round.instruments.clone(),
            reservation: round.reservation,
            ist_day: round.ist_day,
        };
        assert_eq!(
            execute_round_with_clock(&book, round, &mut || (0, DAY)).await,
            RoundOutcome::Canceled
        );
        assert_eq!(entry(&book, 2).rounds, 0);
        assert!(entry(&book, 2).reserved_round.is_none());
        assert!(receiver.try_recv().is_err());
        assert_eq!(
            one_sweep(&book, started + delta() * 3, IN_SESSION, &[1], &[2]).dispatched,
            1
        );
        let current = rounds.try_recv().expect("new reservation");
        assert_ne!(current.reservation, receipt.reservation);
        book.round_finished(&receipt, &[si(2)], Instant::now());
        assert_eq!(
            entry(&book, 2).rounds,
            1,
            "late duplicate refund cannot spend the new round's budget"
        );
        assert_eq!(entry(&book, 2).reserved_round, Some(current.reservation));
        assert_eq!(book.lock().sockets[0].in_flight, Some(current.reservation));
    }

    #[tokio::test]
    async fn test_old_day_rebound_round_cannot_refund_the_new_owner_day() {
        let (book, mut rounds) = SubscribeReconcile::new(true);
        let (old_tx, old_receiver) = socket();
        let started = Instant::now();
        book.record_subscribed_at(&old_tx, &sis(&[1, 2]), started);
        one_sweep(&book, started + delta(), IN_SESSION, &[1], &[2]);
        let old = rounds.try_recv().expect("old day's reservation");
        drop(old_receiver);
        let (new_tx, mut new_receiver) = socket();
        book.record_subscribed_at(&new_tx, &sis(&[1, 2]), started + delta());
        assert_eq!(
            one_sweep_on(
                &book,
                started + delta() * 2,
                IN_SESSION,
                DAY + 1,
                &[1],
                &[2]
            )
            .dispatched,
            1
        );
        let new = rounds.try_recv().expect("new owner's reservation");
        assert_eq!(entry(&book, 2).rounds, 1);
        assert_eq!(
            execute_round_with_clock(&book, old, &mut || (IN_SESSION, DAY + 1)).await,
            RoundOutcome::SendFailed
        );
        assert_eq!(entry(&book, 2).rounds, 1);
        assert_eq!(entry(&book, 2).reserved_round, Some(new.reservation));
        assert_eq!(book.lock().sockets[1].in_flight, Some(new.reservation));
        assert!(
            new_receiver.try_recv().is_err(),
            "old work cannot send on the new socket"
        );
    }

    #[tokio::test]
    async fn test_close_during_channel_capacity_wait_cancels_before_enqueue() {
        use std::future::Future;
        use std::sync::atomic::{AtomicU32, Ordering};
        let (book, mut rounds) = SubscribeReconcile::new(true);
        let (tx, mut receiver) = mpsc::channel(1);
        let started = Instant::now();
        book.record_subscribed_at(&tx, &sis(&[1, 2]), started);
        one_sweep(&book, started + delta(), IN_SESSION, &[1], &[2]);
        let round = rounds.try_recv().expect("reserved round");
        tx.try_send(LiveSubscriptionCommand::Extend {
            more: vec![si(9)],
            ack: None,
        })
        .expect("fill capacity");
        let seconds = AtomicU32::new(IN_SESSION);
        let mut clock = || (seconds.load(Ordering::SeqCst), DAY);
        let execution = execute_round_with_clock(&book, round, &mut clock);
        tokio::pin!(execution);
        std::future::poll_fn(|cx| {
            assert!(
                execution.as_mut().poll(cx).is_pending(),
                "the executor must wait for actual capacity"
            );
            std::task::Poll::Ready(())
        })
        .await;
        seconds.store(0, Ordering::SeqCst);
        assert!(matches!(
            receiver.try_recv().expect("the queued sentinel"),
            LiveSubscriptionCommand::Extend { .. }
        ));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), execution)
                .await
                .expect("bounded cancellation"),
            RoundOutcome::Canceled
        );
        assert!(
            receiver.try_recv().is_err(),
            "the window closed before capacity became available"
        );
        assert_eq!(entry(&book, 2).rounds, 0);
    }

    async fn receive_resubscribe(
        receiver: &mut mpsc::Receiver<LiveSubscriptionCommand>,
    ) -> (
        Vec<SubscribeInstrument>,
        Vec<SubscribeInstrument>,
        oneshot::Sender<ResubscribeOutcome>,
    ) {
        let command = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .expect("bounded fixture receive")
            .expect("socket command");
        let LiveSubscriptionCommand::Resubscribe {
            unsubscribe,
            subscribe,
            ack: Some(ack),
        } = command
        else {
            panic!("the executor must send an acknowledged Resubscribe");
        };
        (unsubscribe, subscribe, ack)
    }

    #[tokio::test]
    async fn test_close_between_split_batches_still_restores_removed_or_unknown_members() {
        use std::sync::atomic::{AtomicU32, Ordering};
        for answered in [true, false] {
            let (book, mut rounds) = SubscribeReconcile::new(true);
            let (tx, mut receiver) = socket();
            let seconds = Arc::new(AtomicU32::new(IN_SESSION));
            let server_seconds = Arc::clone(&seconds);
            let started = Instant::now();
            book.record_subscribed_at(&tx, &sis(&[1, 2, 3]), started);
            one_sweep(&book, started + delta(), IN_SESSION, &[1], &[2, 3]);
            let round = rounds.try_recv().expect("reserved round");
            let server = tokio::spawn(async move {
                let (remove, add, ack) = receive_resubscribe(&mut receiver).await;
                assert_eq!(remove, sis(&[2, 3]));
                assert!(add.is_empty());
                ack.send(ResubscribeOutcome::Refused {
                    reason: ResubscribeOutcome::REASON_NOT_HELD,
                })
                .expect("ack");
                let (remove, add, ack) = receive_resubscribe(&mut receiver).await;
                assert_eq!(remove, sis(&[2]));
                assert!(add.is_empty());
                server_seconds.store(0, Ordering::SeqCst);
                if answered {
                    ack.send(ResubscribeOutcome::Applied).expect("ack");
                } else {
                    drop(ack);
                }
                let (remove, add, ack) = receive_resubscribe(&mut receiver).await;
                assert!(
                    remove.is_empty(),
                    "closed window forbids the remaining unsubscribe"
                );
                assert_eq!(
                    add,
                    sis(&[2]),
                    "the already removed member must still be restored"
                );
                ack.send(ResubscribeOutcome::Applied).expect("ack");
                assert!(receiver.try_recv().is_err());
            });
            let outcome = execute_round_with_clock(&book, round, &mut || {
                (seconds.load(Ordering::SeqCst), DAY)
            })
            .await;
            server.await.expect("fixture assertions");
            assert_eq!(
                outcome,
                RoundOutcome::Resubscribed {
                    resent: 1,
                    not_held: 0,
                    unplaced: 0
                }
            );
            assert_eq!(entry(&book, 2).rounds, 1);
            assert_eq!(entry(&book, 3).rounds, 0);
        }
    }

    #[tokio::test]
    async fn test_not_held_reply_cancels_new_placement_after_forget_or_close() {
        use std::sync::atomic::{AtomicU32, Ordering};
        for forget in [true, false] {
            let (book, mut rounds) = SubscribeReconcile::new(true);
            let (tx, mut receiver) = socket();
            let seconds = AtomicU32::new(IN_SESSION);
            let started = Instant::now();
            book.record_subscribed_at(&tx, &sis(&[1, 2]), started);
            one_sweep(&book, started + delta(), IN_SESSION, &[1], &[2]);
            let round = rounds.try_recv().expect("reserved round");
            let mut clock = || (seconds.load(Ordering::SeqCst), DAY);
            let execution = execute_round_with_clock(&book, round, &mut clock);
            let server = async {
                let (remove, add, ack) = receive_resubscribe(&mut receiver).await;
                assert_eq!(remove, sis(&[2]));
                assert!(add.is_empty());
                if forget {
                    book.forget_on(&tx, &[si(2)]);
                } else {
                    seconds.store(0, Ordering::SeqCst);
                }
                ack.send(ResubscribeOutcome::Refused {
                    reason: ResubscribeOutcome::REASON_NOT_HELD,
                })
                .expect("not-held reply");
            };
            let (outcome, ()) = tokio::join!(
                tokio::time::timeout(Duration::from_secs(5), execution),
                server
            );
            assert_eq!(
                outcome.expect("bounded cancellation"),
                RoundOutcome::Canceled
            );
            assert!(
                receiver.try_recv().is_err(),
                "known-not-held placement is new work, not restoration"
            );
            assert!(book.lock().sockets[0].in_flight.is_none());
            if forget {
                assert_eq!(book.expected_len(), 1);
            } else {
                assert_eq!(entry(&book, 2).rounds, 0);
            }
        }
    }

    #[tokio::test]
    async fn test_not_held_placement_retry_crossing_close_cancels_and_refunds() {
        use std::sync::atomic::{AtomicU32, Ordering};
        tokio::time::pause();
        let (book, mut rounds) = SubscribeReconcile::new(true);
        let (tx, mut receiver) = socket();
        let seconds = AtomicU32::new(IN_SESSION);
        let started = Instant::now();
        book.record_subscribed_at(&tx, &sis(&[1, 2]), started);
        one_sweep(&book, started + delta(), IN_SESSION, &[1], &[2]);
        let round = rounds.try_recv().expect("reserved round");
        let mut clock = || (seconds.load(Ordering::SeqCst), DAY);
        let execution = execute_round_with_clock(&book, round, &mut clock);
        let server = async {
            let (remove, add, ack) = receive_resubscribe(&mut receiver).await;
            assert_eq!(remove, sis(&[2]));
            assert!(add.is_empty());
            ack.send(ResubscribeOutcome::Refused {
                reason: ResubscribeOutcome::REASON_NOT_HELD,
            })
            .expect("not-held reply");
            let (remove, add, ack) = receive_resubscribe(&mut receiver).await;
            assert!(remove.is_empty());
            assert_eq!(add, sis(&[2]));
            ack.send(ResubscribeOutcome::Refused {
                reason: ResubscribeOutcome::REASON_OVER_CAP,
            })
            .expect("definite placement refusal");
            // Cross the close during the two-second retry pause, after the
            // first eligible placement has definitely failed.
            tokio::time::sleep(Duration::from_secs(1)).await;
            seconds.store(0, Ordering::SeqCst);
        };
        let (outcome, ()) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(5), execution),
            server
        );
        assert_eq!(
            outcome.expect("bounded retry cancellation"),
            RoundOutcome::Canceled
        );
        assert!(receiver.try_recv().is_err(), "no stale placement retry");
        assert_eq!(entry(&book, 2).rounds, 0);
        assert!(entry(&book, 2).reserved_round.is_none());
    }

    #[tokio::test]
    async fn test_partial_not_held_placement_refunds_only_the_canceled_retry_tail() {
        use std::sync::atomic::{AtomicU32, Ordering};
        tokio::time::pause();
        let (book, mut rounds) = SubscribeReconcile::new(true);
        let (tx, mut receiver) = socket();
        let seconds = AtomicU32::new(IN_SESSION);
        let started = Instant::now();
        book.record_subscribed_at(&tx, &sis(&[1, 2, 3]), started);
        one_sweep(&book, started + delta(), IN_SESSION, &[1], &[2, 3]);
        let round = rounds.try_recv().expect("reserved round");
        let mut clock = || (seconds.load(Ordering::SeqCst), DAY);
        let execution = execute_round_with_clock(&book, round, &mut clock);
        let server = async {
            for expected in [sis(&[2, 3]), sis(&[2]), sis(&[3])] {
                let (remove, add, ack) = receive_resubscribe(&mut receiver).await;
                assert_eq!(remove, expected);
                assert!(add.is_empty());
                ack.send(ResubscribeOutcome::Refused {
                    reason: ResubscribeOutcome::REASON_NOT_HELD,
                })
                .expect("not-held reply");
            }
            let (remove, add, ack) = receive_resubscribe(&mut receiver).await;
            assert!(remove.is_empty());
            assert_eq!(add, sis(&[2, 3]));
            ack.send(ResubscribeOutcome::Stopped {
                not_unsubscribed: Vec::new(),
                not_subscribed: sis(&[3]),
            })
            .expect("only instrument 2 was placed");
            tokio::time::sleep(Duration::from_secs(1)).await;
            seconds.store(0, Ordering::SeqCst);
        };
        let (outcome, ()) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(5), execution),
            server
        );
        assert_eq!(
            outcome.expect("bounded partial retry cancellation"),
            RoundOutcome::Resubscribed {
                resent: 1,
                not_held: 2,
                unplaced: 1,
            }
        );
        assert!(receiver.try_recv().is_err(), "no stale tail retry");
        assert_eq!(
            entry(&book, 2).rounds,
            1,
            "the placed member spent its round"
        );
        assert_eq!(entry(&book, 3).rounds, 0, "the canceled tail is refunded");
    }

    #[test]
    fn test_reservation_identity_exhaustion_refuses_instead_of_reusing_an_id() {
        let (book, mut rounds) = SubscribeReconcile::new(true);
        let (tx, _receiver) = socket();
        let started = Instant::now();
        book.record_subscribed_at(&tx, &sis(&[1, 2]), started);
        book.lock().next_reservation = u64::MAX;
        assert_eq!(
            one_sweep(&book, started + delta(), IN_SESSION, &[1], &[2]).dispatched,
            0
        );
        assert!(rounds.try_recv().is_err());
        assert_eq!(entry(&book, 2).rounds, 0);
        assert!(entry(&book, 2).reserved_round.is_none());
    }

    /// Cost the reconcile adds to the drain's 30 s silence scan at the
    /// authorized ceiling: 25,000 expected instruments, every report noted,
    /// one sweep finished. Wall clock, so `#[ignore]`d (never a CI gate):
    /// `cargo test -p tickvault-app --release --lib -- --ignored sweep_cost_at_the_authorized_ceiling --nocapture`.
    #[test]
    #[ignore = "wall-clock cost harness, run by hand in release; never a CI gate"]
    #[expect(
        clippy::print_stderr,
        reason = "this explicit ignored measurement harness reports its measured sweep cost on stderr"
    )]
    fn sweep_cost_at_the_authorized_ceiling() {
        let (book, _rx) = SubscribeReconcile::new(false);
        let mut keep = Vec::new();
        let t0 = Instant::now();
        let per_socket = RECONCILE_MAX_EXPECTED / 5;
        for s in 0..5u64 {
            let (tx, rx) = socket();
            let ids: Vec<SubscribeInstrument> = (0..per_socket as u64)
                .map(|i| si(s * per_socket as u64 + i))
                .collect();
            book.record_subscribed_at(&tx, &ids, t0);
            keep.push((tx, rx));
        }
        // Worst shape: one in five never ticked (capped per socket), the rest
        // live.
        let reports: Vec<SilenceReport> = (0..RECONCILE_MAX_EXPECTED as u64)
            .map(|id| {
                report(
                    id,
                    if id % 5 == 0 {
                        SilenceVerdict::NeverTicked
                    } else {
                        SilenceVerdict::Live
                    },
                )
            })
            .collect();
        let rounds = 50u32;
        let mut total = Duration::ZERO;
        for r in 0..rounds {
            let at = t0 + delta() + Duration::from_secs(u64::from(r) * 30);
            let start = Instant::now();
            let mut sweep = book.sweep_at(at);
            for report in &reports {
                sweep.note(report);
            }
            let s = sweep.finish_at(IN_SESSION, DAY);
            total += start.elapsed();
            assert_eq!(s.missing, (RECONCILE_MAX_EXPECTED / 5) as u32);
        }
        let mean = total / rounds;
        let per = mean.as_nanos() / RECONCILE_MAX_EXPECTED as u128;
        eprintln!(
            "subscribe reconcile sweep at {} reports: mean {:?} per sweep, {} ns per report",
            RECONCILE_MAX_EXPECTED, mean, per
        );
        assert!(mean < Duration::from_secs(1));
    }
}

/// The silence sweep's slot-cell fast path (2026-11-17): `note_at_slot` must
/// reach exactly the state `note` reaches, and no cell may ever serve a copy
/// that a forget, a rebind or a re-insert made stale.
#[cfg(test)]
mod slot_cell_tests {
    use super::*;
    use proptest::prelude::*;
    use tickvault_core::pipeline::tick_gap_detector::{DetectorConfig, TickGapDetector};

    const IN_SESSION: u32 = 10 * 3600;
    const DAY: i64 = 20_000;

    fn si(id: u64) -> SubscribeInstrument {
        SubscribeInstrument {
            security_id: id,
            segment: ExchangeSegment::NseFno,
        }
    }

    fn report(id: u64, verdict: SilenceVerdict) -> SilenceReport {
        SilenceReport {
            key: (id, ExchangeSegment::NseFno),
            silent_millis: 0,
            expected_millis: 0,
            baseline_millis: 0,
            samples: 0,
            verdict,
            sparse: false,
            warming_up: false,
            alarm_sparse: false,
        }
    }

    fn socket() -> (
        mpsc::Sender<LiveSubscriptionCommand>,
        mpsc::Receiver<LiveSubscriptionCommand>,
    ) {
        mpsc::channel(4)
    }

    fn delta() -> Duration {
        Duration::from_secs(RECONCILE_DELTA_SECS)
    }

    /// Runs one sweep through `note_at_slot` with `(slot, report)` pairs.
    fn slot_sweep(
        book: &SubscribeReconcile,
        at: Instant,
        reports: &[(u32, SilenceReport)],
    ) -> SweepSummary {
        let mut sweep = book.sweep_at(at);
        for (slot, r) in reports {
            sweep.note_at_slot(*slot, r);
        }
        sweep.finish_at(IN_SESSION, DAY)
    }

    #[test]
    fn test_forget_empties_the_cached_cell() {
        let (book, _rounds) = SubscribeReconcile::new(false);
        let (tx, _rx) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &[si(1), si(2)], t0);
        slot_sweep(
            &book,
            t0,
            &[
                (4, report(1, SilenceVerdict::Live)),
                (9, report(2, SilenceVerdict::Live)),
            ],
        );
        assert_eq!(book.cached_cells(), 2, "the first sweep caches both");
        book.assert_cell_invariant();
        book.forget(&[si(1)]);
        assert_eq!(book.cached_cells(), 1, "forget empties its cell");
        book.assert_cell_invariant();
        book.forget_on(&tx, &[si(2)]);
        assert_eq!(book.cached_cells(), 0, "forget_on empties its cell");
        book.assert_cell_invariant();
    }

    #[test]
    fn test_rebind_empties_the_cached_cell() {
        let (book, _rounds) = SubscribeReconcile::new(false);
        let (tx_a, rx_a) = socket();
        let (tx_b, _rx_b) = socket();
        let (tx_c, _rx_c) = socket();
        let t0 = Instant::now();
        // Socket B delivers instrument 9, so the feed and socket gates pass.
        book.record_subscribed_at(&tx_a, &[si(1)], t0);
        book.record_subscribed_at(&tx_b, &[si(9)], t0);
        slot_sweep(&book, t0, &[(0, report(1, SilenceVerdict::NeverTicked))]);
        assert_eq!(book.cached_cells(), 1);
        // Socket A closes; instrument 1 is re-subscribed on C, 10 minutes on.
        drop(rx_a);
        let rebound = t0 + Duration::from_secs(600);
        book.record_subscribed_at(&tx_c, &[si(1)], rebound);
        assert_eq!(book.cached_cells(), 0, "the rebind empties the stale cell");
        book.assert_cell_invariant();
        // 60 s after the rebind: under the delta against the NEW record time,
        // so not missing. A stale cell (record time t0) would say missing.
        let at = rebound + Duration::from_secs(60);
        let summary = slot_sweep(
            &book,
            at,
            &[
                (0, report(1, SilenceVerdict::NeverTicked)),
                (1, report(9, SilenceVerdict::Live)),
            ],
        );
        assert_eq!(summary.missing, 0);
        book.assert_cell_invariant();
        // Re-cached with the new socket: the next sweep past the delta counts
        // it on socket C.
        let at = rebound + delta();
        let summary = slot_sweep(
            &book,
            at,
            &[
                (0, report(1, SilenceVerdict::NeverTicked)),
                (1, report(9, SilenceVerdict::Live)),
            ],
        );
        assert_eq!(summary.missing, 1);
        let inner = book.lock();
        assert_eq!(inner.expected[&(1, ExchangeSegment::NseFno)].socket, 2);
        assert_eq!(inner.cells[0].socket, 2, "cached on the rebound socket");
    }

    #[test]
    fn test_reinserted_key_is_probed_not_served_from_a_stale_cell() {
        let (book, _rounds) = SubscribeReconcile::new(false);
        let (tx, _rx) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &[si(1)], t0);
        slot_sweep(&book, t0, &[(7, report(1, SilenceVerdict::NeverTicked))]);
        assert_eq!(book.cached_cells(), 1);
        book.forget(&[si(1)]);
        let again = t0 + Duration::from_secs(1_000);
        book.record_subscribed_at(&tx, &[si(1)], again);
        // 30 s after the re-insert: a stale cell would carry t0 and count it
        // missing; the probe reads the new record time and does not.
        let mut sweep = book.sweep_at(again + Duration::from_secs(30));
        sweep.note_at_slot(7, &report(1, SilenceVerdict::NeverTicked));
        assert_eq!(sweep.missing, 0);
        drop(sweep);
        assert_eq!(book.cached_cells(), 1, "re-cached from the fresh entry");
        book.assert_cell_invariant();
        assert_eq!(book.lock().cells[7].recorded_at, again);
    }

    #[test]
    fn test_rounds_any_forces_the_probe_until_the_day_reset() {
        let (book, mut rounds) = SubscribeReconcile::new(true);
        let (tx, _rx) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &[si(1), si(2)], t0);
        let at = t0 + delta();
        let summary = slot_sweep(
            &book,
            at,
            &[
                (0, report(1, SilenceVerdict::Live)),
                (1, report(2, SilenceVerdict::NeverTicked)),
            ],
        );
        assert_eq!(summary.dispatched, 1, "a round is reserved");
        assert!(book.lock().rounds_any);
        let round = rounds.try_recv().expect("the round is queued");
        book.round_finished(&round, &[], at);
        // Instrument 2 now has a spent round. Its first packet must be seen as
        // a recovery, which only the probe (`note`) records: the fast path
        // would skip `mark_recovered`.
        let at = at + Duration::from_secs(10);
        let mut sweep = book.sweep_at(at);
        sweep.note_at_slot(1, &report(2, SilenceVerdict::Live));
        sweep.finish_at(IN_SESSION, DAY);
        assert!(
            book.lock().expected[&(2, ExchangeSegment::NseFno)].recovered_reported,
            "rounds_any made the sweep take the probe"
        );
        book.assert_cell_invariant();
        // The next IST day gives the rounds back and clears the flag.
        let sweep = book.sweep_at(at);
        sweep.finish_at(IN_SESSION, DAY + 1);
        assert!(!book.lock().rounds_any, "the day reset clears rounds_any");
    }

    #[test]
    fn test_cell_with_another_key_falls_back_to_the_probe() {
        let (book, _rounds) = SubscribeReconcile::new(false);
        let (tx, _rx) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &[si(1), si(2)], t0);
        slot_sweep(&book, t0, &[(3, report(1, SilenceVerdict::Live))]);
        assert_eq!(
            book.lock().expected[&(1, ExchangeSegment::NseFno)].view_slot,
            3
        );
        // The detector now hands slot 3 to instrument 2 (a session reset
        // reassigned slots). Past the delta, never ticked: the probe judges it.
        let mut sweep = book.sweep_at(t0 + delta());
        sweep.note_at_slot(3, &report(2, SilenceVerdict::NeverTicked));
        assert_eq!(sweep.missing, 1, "judged by the probe, not by key 1's cell");
        drop(sweep);
        let inner = book.lock();
        assert_eq!(inner.cells[3].security_id, 2, "the cell now holds key 2");
        assert_eq!(
            inner.expected[&(1, ExchangeSegment::NseFno)].view_slot,
            VIEW_SLOT_NONE,
            "key 1 lost the slot"
        );
        drop(inner);
        book.assert_cell_invariant();
    }

    #[test]
    fn test_slot_past_cell_capacity_probes() {
        let (book, _rounds) = SubscribeReconcile::new(false);
        let (tx, _rx) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &[si(1)], t0);
        let past = u32::try_from(RECONCILE_CELL_SLOTS).unwrap_or(u32::MAX);
        for slot in [past, past + 1, u32::MAX] {
            let mut sweep = book.sweep_at(t0);
            sweep.note_at_slot(slot, &report(1, SilenceVerdict::Live));
            assert_eq!(sweep.inner.sockets[0].delivering, 1, "slot {slot} probed");
            drop(sweep);
            assert_eq!(book.cached_cells(), 0, "slot {slot} caches nothing");
        }
        book.assert_cell_invariant();
    }

    #[test]
    fn test_cell_capacity_equals_detector_capacity() {
        let detector =
            TickGapDetector::with_capacity(AGGREGATOR_MAX_SLOTS, DetectorConfig::live_lane());
        assert_eq!(RECONCILE_CELL_SLOTS, detector.capacity());
        let (book, _rounds) = SubscribeReconcile::new(false);
        assert_eq!(book.lock().cells.len(), RECONCILE_CELL_SLOTS);
        // That the live lane builds its detector at exactly this capacity is
        // pinned by `crates/app/tests/silence_arm_slot_guard.rs`.
    }

    #[test]
    fn test_record_subscribed_at_inserts_with_no_cell_and_keeps_the_first_live_socket() {
        let (book, _rounds) = SubscribeReconcile::new(false);
        let (tx_a, _rx_a) = socket();
        let (tx_b, _rx_b) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx_a, &[si(1)], t0);
        book.record_subscribed_at(&tx_b, &[si(1)], t0 + delta());
        let inner = book.lock();
        let e = inner.expected[&(1, ExchangeSegment::NseFno)];
        assert_eq!(e.view_slot, VIEW_SLOT_NONE, "a new entry holds no cell");
        assert_eq!(e.socket, 0, "a live first socket is kept");
        assert_eq!(e.recorded_at, t0);
    }

    #[test]
    fn test_assert_cell_invariant_holds_and_bites_on_a_stale_cell() {
        let (book, _rounds) = SubscribeReconcile::new(false);
        book.assert_cell_invariant();
        let (tx, _rx) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &[si(1)], t0);
        slot_sweep(&book, t0, &[(2, report(1, SilenceVerdict::Live))]);
        book.assert_cell_invariant();
        // Corrupt the cell's record time: the check must refuse it.
        book.lock().cells[2].recorded_at = t0 + delta();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            book.assert_cell_invariant();
        }));
        assert!(caught.is_err(), "a stale cell must fail the invariant");
    }

    #[test]
    fn test_cached_cells_counts_valid_cells_only() {
        let (book, _rounds) = SubscribeReconcile::new(false);
        assert_eq!(book.cached_cells(), 0);
        let (tx, _rx) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &[si(1), si(2), si(3)], t0);
        slot_sweep(
            &book,
            t0,
            &[
                (0, report(1, SilenceVerdict::Live)),
                (1, report(2, SilenceVerdict::Exceeded)),
                // Not expected: probed, nothing cached.
                (2, report(77, SilenceVerdict::Live)),
            ],
        );
        assert_eq!(book.cached_cells(), 2);
        // The same key at another slot moves, it does not duplicate.
        slot_sweep(&book, t0, &[(5, report(1, SilenceVerdict::Live))]);
        assert_eq!(book.cached_cells(), 2);
        book.assert_cell_invariant();
    }

    // ---------------------------------------------------------------- proptest

    /// One step applied to both books.
    #[derive(Debug, Clone)]
    enum Op {
        Record {
            socket: usize,
            keys: Vec<usize>,
            at_ms: u64,
        },
        CloseSocket {
            socket: usize,
        },
        Forget {
            keys: Vec<usize>,
        },
        ForgetOn {
            socket: usize,
            keys: Vec<usize>,
        },
        FinishRounds {
            refund_mask: u8,
            at_ms: u64,
        },
        Sweep {
            at_ms: u64,
            secs: u32,
            day: i64,
            reports: Vec<(usize, u8, u64, u64, u8, u32)>,
        },
    }

    const KEYS: [(u64, ExchangeSegment); 8] = [
        (1, ExchangeSegment::NseFno),
        (1, ExchangeSegment::NseEquity),
        (2, ExchangeSegment::NseFno),
        (2, ExchangeSegment::IdxI),
        (3, ExchangeSegment::NseFno),
        (4, ExchangeSegment::NseFno),
        (5, ExchangeSegment::NseEquity),
        (6, ExchangeSegment::NseFno),
    ];

    fn instruments(keys: &[usize]) -> Vec<SubscribeInstrument> {
        keys.iter()
            .map(|k| SubscribeInstrument {
                security_id: KEYS[*k].0,
                segment: KEYS[*k].1,
            })
            .collect()
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        let keys = || prop::collection::vec(0usize..KEYS.len(), 0..6);
        prop_oneof![
            4 => (0usize..5, keys(), 0u64..900_000)
                .prop_map(|(socket, keys, at_ms)| Op::Record { socket, keys, at_ms }),
            1 => (0usize..5).prop_map(|socket| Op::CloseSocket { socket }),
            1 => keys().prop_map(|keys| Op::Forget { keys }),
            1 => (0usize..5, keys()).prop_map(|(socket, keys)| Op::ForgetOn { socket, keys }),
            2 => (any::<u8>(), 0u64..900_000)
                .prop_map(|(refund_mask, at_ms)| Op::FinishRounds { refund_mask, at_ms }),
            6 => (
                0u64..900_000,
                prop_oneof![Just(IN_SESSION), Just(5 * 3600u32), 0u32..86_400],
                prop_oneof![4 => Just(DAY), 1 => Just(DAY + 1), 1 => DAY - 2..DAY + 3],
                prop::collection::vec(
                    (0usize..KEYS.len(), 0u8..4, 0u64..100, 0u64..100, 0u8..10, 0u32..40),
                    0..14,
                ),
            )
                .prop_map(|(at_ms, secs, day, reports)| Op::Sweep {
                    at_ms,
                    secs,
                    day,
                    reports,
                }),
        ]
    }

    /// The slot a report arrives at: mostly the key's own stable slot, some
    /// a shared small slot (another key's cell), some past the cell table.
    fn slot_of(key: usize, choice: u8, random: u32) -> u32 {
        let past = u32::try_from(RECONCILE_CELL_SLOTS).unwrap_or(u32::MAX);
        match choice {
            0..=6 => u32::try_from(key * 3).unwrap_or(0),
            7 | 8 => random % 8,
            _ => past + random,
        }
    }

    fn verdict_of(code: u8) -> SilenceVerdict {
        match code {
            0 => SilenceVerdict::NeverTicked,
            1 => SilenceVerdict::Warming,
            2 => SilenceVerdict::Live,
            _ => SilenceVerdict::Exceeded,
        }
    }

    /// Everything the sweep can change, minus the cells and `view_slot`
    /// (which only book B keeps).
    /// One entry: key, socket, record time, rounds, reservation, then the
    /// missing / exhausted / recovered flags.
    type EntryView = (Key, u8, Instant, u8, Option<u64>, bool, bool, bool);
    /// One socket: last round, in-flight round, delivering, candidates, silent.
    type SocketView = (Option<Instant>, Option<u64>, u32, Vec<Key>, bool);

    #[derive(Debug, PartialEq)]
    struct Observed {
        entries: Vec<EntryView>,
        sockets: Vec<SocketView>,
        exhausted_named: Vec<Key>,
        ist_day: i64,
        feed_silent_reported: bool,
        next_reservation: u64,
    }

    fn observe(book: &SubscribeReconcile) -> Observed {
        let inner = book.lock();
        let mut entries: Vec<_> = inner
            .expected
            .iter()
            .map(|(k, e)| {
                (
                    *k,
                    e.socket,
                    e.recorded_at,
                    e.rounds,
                    e.reserved_round,
                    e.missing_counted,
                    e.exhausted_reported,
                    e.recovered_reported,
                )
            })
            .collect();
        entries.sort_by_key(|e| (e.0.0, e.0.1.as_str()));
        Observed {
            entries,
            sockets: inner
                .sockets
                .iter()
                .map(|s| {
                    (
                        s.last_round_at,
                        s.in_flight,
                        s.delivering,
                        s.candidates.iter().map(key_of).collect(),
                        s.silent_reported,
                    )
                })
                .collect(),
            exhausted_named: inner.exhausted_named.iter().map(key_of).collect(),
            ist_day: inner.ist_day,
            feed_silent_reported: inner.feed_silent_reported,
            next_reservation: inner.next_reservation,
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Book A judges every report with `note`; book B with `note_at_slot`
        /// at a random slot. Over random records on up to five sockets
        /// (closing some to force rebinds), forgets, rounds finished with
        /// random refunds, sweeps at instants moving forward and backward,
        /// IST-day changes and in- and out-of-window clocks, shadow and auto
        /// mode, the two books must agree on every summary, every round handed
        /// to the executor, every entry, every socket and the exhausted names;
        /// and book B's cells must keep their invariant after every step.
        #[test]
        fn proptest_note_at_slot_matches_note(
            auto in any::<bool>(),
            ops in prop::collection::vec(op_strategy(), 1..60),
        ) {
            let (a, mut a_rounds) = SubscribeReconcile::new(auto);
            let (b, mut b_rounds) = SubscribeReconcile::new(auto);
            let base = Instant::now();
            let at = |ms: u64| base + Duration::from_millis(ms);
            let mut sockets: Vec<(
                mpsc::Sender<LiveSubscriptionCommand>,
                Option<mpsc::Receiver<LiveSubscriptionCommand>>,
            )> = (0..5)
                .map(|_| {
                    let (tx, rx) = socket();
                    (tx, Some(rx))
                })
                .collect();
            for op in ops {
                match op {
                    Op::Record { socket, keys, at_ms } => {
                        let list = instruments(&keys);
                        a.record_subscribed_at(&sockets[socket].0, &list, at(at_ms));
                        b.record_subscribed_at(&sockets[socket].0, &list, at(at_ms));
                    }
                    Op::CloseSocket { socket } => {
                        // The receiver goes, so the next record of its
                        // instruments elsewhere rebinds them; the slot gets a
                        // fresh channel.
                        sockets[socket].1 = None;
                        let (tx, rx) = self::socket();
                        let old = std::mem::replace(&mut sockets[socket], (tx, Some(rx)));
                        drop(old);
                    }
                    Op::Forget { keys } => {
                        let list = instruments(&keys);
                        a.forget(&list);
                        b.forget(&list);
                    }
                    Op::ForgetOn { socket, keys } => {
                        let list = instruments(&keys);
                        a.forget_on(&sockets[socket].0, &list);
                        b.forget_on(&sockets[socket].0, &list);
                    }
                    Op::FinishRounds { refund_mask, at_ms } => loop {
                        let ra = a_rounds.try_recv();
                        let rb = b_rounds.try_recv();
                        match (ra, rb) {
                            (Ok(ra), Ok(rb)) => {
                                prop_assert_eq!(&ra.instruments, &rb.instruments);
                                prop_assert_eq!(ra.socket, rb.socket);
                                prop_assert_eq!(ra.reservation, rb.reservation);
                                prop_assert_eq!(ra.ist_day, rb.ist_day);
                                let refund: Vec<SubscribeInstrument> = ra
                                    .instruments
                                    .iter()
                                    .enumerate()
                                    .filter(|(i, _)| refund_mask & (1u8 << (i % 8)) != 0)
                                    .map(|(_, x)| *x)
                                    .collect();
                                a.round_finished(&ra, &refund, at(at_ms));
                                b.round_finished(&rb, &refund, at(at_ms));
                            }
                            (Err(_), Err(_)) => break,
                            (ra, rb) => {
                                prop_assert!(false, "rounds differ: {:?} vs {:?}", ra.is_ok(), rb.is_ok());
                            }
                        }
                    },
                    Op::Sweep { at_ms, secs, day, reports } => {
                        let mut sa = a.sweep_at(at(at_ms));
                        let mut sb = b.sweep_at(at(at_ms));
                        for (key, verdict, silent, expected, choice, random) in reports {
                            let r = SilenceReport {
                                key: KEYS[key],
                                silent_millis: silent,
                                expected_millis: expected,
                                baseline_millis: 0,
                                samples: 0,
                                verdict: verdict_of(verdict),
                                sparse: false,
                                warming_up: false,
                                alarm_sparse: false,
                            };
                            let slot = slot_of(key, choice, random);
                            sa.note(&r);
                            sb.note_at_slot(slot, &r);
                            prop_assert_eq!(sa.missing, sb.missing);
                            prop_assert_eq!(sa.exhausted, sb.exhausted);
                        }
                        let summary_a = sa.finish_at(secs, day);
                        let summary_b = sb.finish_at(secs, day);
                        prop_assert_eq!(summary_a, summary_b);
                    }
                }
                b.assert_cell_invariant();
                prop_assert_eq!(observe(&a), observe(&b));
            }
            // Book A never caches.
            prop_assert_eq!(a.cached_cells(), 0);
        }
    }

    /// Non-vacuity for the proptest above: with the seeds proptest draws, a
    /// steady run takes the fast path, so the equality is not only ever
    /// checked on the probe. Deterministic so it cannot be lucky.
    #[test]
    fn test_fast_path_hits_count_each_fast_path_report_and_note_agrees() {
        let (a, _ra) = SubscribeReconcile::new(false);
        let (b, _rb) = SubscribeReconcile::new(false);
        let (tx, _rx) = socket();
        let t0 = Instant::now();
        let list: Vec<_> = (0..50).map(si).collect();
        a.record_subscribed_at(&tx, &list, t0);
        b.record_subscribed_at(&tx, &list, t0);
        for sweep_n in 0..4u64 {
            let at = t0 + Duration::from_secs(60 * sweep_n);
            let mut sa = a.sweep_at(at);
            let mut sb = b.sweep_at(at);
            let mut hits = 0;
            for id in 0..50u64 {
                let r = report(
                    id,
                    match id % 4 {
                        0 => SilenceVerdict::NeverTicked,
                        1 => SilenceVerdict::Warming,
                        2 => SilenceVerdict::Live,
                        _ => SilenceVerdict::Exceeded,
                    },
                );
                let slot = u32::try_from(id).unwrap_or(0);
                if sb.inner.cells[id as usize].socket != CELL_EMPTY {
                    hits += 1;
                }
                sa.note(&r);
                sb.note_at_slot(slot, &r);
            }
            assert_eq!(hits, if sweep_n == 0 { 0 } else { 50 }, "sweep {sweep_n}");
            // The count of fast-path decisions, not only valid cells: the slow
            // path re-fills the same cells, so `hits` alone cannot tell them apart.
            assert_eq!(
                sb.fast_hits,
                if sweep_n == 0 { 0 } else { 50 },
                "fast-path decisions, sweep {sweep_n}"
            );
            assert_eq!(sa.finish_at(IN_SESSION, DAY), sb.finish_at(IN_SESSION, DAY));
            assert_eq!(observe(&a), observe(&b));
            b.assert_cell_invariant();
        }
        assert_eq!(
            b.fast_path_hits(),
            150,
            "three steady sweeps of 50 on the fast path"
        );
        assert_eq!(a.fast_path_hits(), 0, "note never takes the fast path");
    }

    /// A key the book does not expect: `note` reports the miss, and
    /// `note_at_slot` caches nothing and never counts a fast-path hit, so it
    /// pays one probe per sweep, not two (2026-11-17 review).
    #[test]
    fn test_unexpected_key_is_probed_once_and_never_cached() {
        let (book, _rounds) = SubscribeReconcile::new(false);
        let (tx, _rx) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &[si(1)], t0);
        for sweep_n in 0..3u64 {
            let mut sweep = book.sweep_at(t0 + Duration::from_secs(60 * sweep_n));
            assert!(
                !sweep.note(&report(7, SilenceVerdict::Live)),
                "not expected"
            );
            assert!(sweep.note(&report(1, SilenceVerdict::Live)), "expected");
            sweep.note_at_slot(7, &report(7, SilenceVerdict::Live));
            assert_eq!(sweep.fast_hits, 0, "an unexpected key never hits a cell");
            let _ = sweep.finish_at(IN_SESSION, DAY);
            assert_eq!(book.cached_cells(), 0, "an unexpected key gets no cell");
        }
        assert_eq!(book.fast_path_hits(), 0);
    }
}

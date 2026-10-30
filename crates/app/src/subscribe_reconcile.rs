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
//! - The whole feed must be delivering NOW: at least one expected instrument
//!   reads `Live` or `Warming`. An instrument that ticked earlier and has gone
//!   quiet (`Exceeded`) does not count, so a halt is not a partial subscribe.
//! - The socket must be delivering now for at least one other expected
//!   instrument; a socket where nothing is live is the frame-silence redial's
//!   job, counted here as `skipped_silent_socket` once per episode.
//!
//! ## Complexity
//!
//! `note` is O(1) per report: one SipHash probe into a map of at most
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

/// Counter: one per round decision or executor result, by `outcome`.
pub const RECONCILE_ROUNDS_METRIC: &str = "tv_subscribe_reconcile_rounds_total";

/// Counter: instruments by what happened to them, by `outcome`.
pub const RECONCILE_INSTRUMENTS_METRIC: &str = "tv_subscribe_reconcile_instruments_total";

/// Gauge: expected instruments missing at the last sweep.
pub const RECONCILE_MISSING_GAUGE: &str = "tv_subscribe_reconcile_missing";

/// Every `outcome` label of [`RECONCILE_ROUNDS_METRIC`], registered at 0.
const ROUND_OUTCOMES: [&str; 14] = [
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
];

/// Every `outcome` label of [`RECONCILE_INSTRUMENTS_METRIC`], registered at 0.
const INSTRUMENT_OUTCOMES: [&str; 8] = [
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

fn key_of(instrument: &SubscribeInstrument) -> Key {
    (instrument.security_id, instrument.segment)
}

/// One expected instrument.
#[derive(Debug, Clone, Copy)]
struct Expected {
    instrument: SubscribeInstrument,
    socket: u8,
    recorded_at: Instant,
    /// Rounds actually sent today (shadow mode never spends one).
    rounds: u8,
    /// Counted on `missing_found` today.
    missing_counted: bool,
    exhausted_reported: bool,
    recovered_reported: bool,
}

/// One socket the lane subscribed on.
#[derive(Debug)]
struct Socket {
    tx: mpsc::Sender<LiveSubscriptionCommand>,
    /// When the last round on this socket finished (shadow: when it was
    /// planned).
    last_round_at: Option<Instant>,
    /// A round for this socket is queued or running.
    in_flight: bool,
    /// Expected instruments delivering now (`Live` or `Warming`) this sweep.
    delivering: u32,
    candidates: Vec<SubscribeInstrument>,
    silent_reported: bool,
}

#[derive(Debug)]
struct Inner {
    sockets: Vec<Socket>,
    expected: HashMap<Key, Expected>,
    /// The first [`NAMED_IN_LOG`] instruments that used their last round in
    /// the current sweep, for the log line. Pre-sized once and cleared per
    /// sweep, so `note` never allocates; the full count is kept apart.
    exhausted_named: Vec<SubscribeInstrument>,
    ist_day: i64,
    feed_silent_reported: bool,
}

/// One planned resend, handed to the executor.
#[derive(Debug)]
pub struct PlannedRound {
    tx: mpsc::Sender<LiveSubscriptionCommand>,
    socket: u8,
    instruments: Vec<SubscribeInstrument>,
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
                expected: HashMap::with_capacity(RECONCILE_MAX_EXPECTED),
                exhausted_named: Vec::with_capacity(NAMED_IN_LOG),
                ist_day: ist_day_now(),
                feed_silent_reported: false,
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
    /// main-feed socket. An instrument already recorded keeps its first socket
    /// (a backup copy on a second socket is not the copy being reconciled).
    ///
    /// Cold path: O(instruments) once per subscribe command.
    pub fn record_subscribed(
        &self,
        tx: &mpsc::Sender<LiveSubscriptionCommand>,
        instruments: &[SubscribeInstrument],
    ) {
        self.record_subscribed_at(tx, instruments, Instant::now());
    }

    fn record_subscribed_at(
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
                    in_flight: false,
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
            if inner.expected.contains_key(&key) {
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
                    missing_counted: false,
                    exhausted_reported: false,
                    recovered_reported: false,
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
        for instrument in instruments {
            inner.expected.remove(&key_of(instrument));
        }
    }

    /// Instruments currently expected.
    #[must_use]
    pub fn expected_len(&self) -> usize {
        self.lock().expected.len()
    }

    /// Starts one judging pass. Feed it every report of the drain's silence
    /// sweep with [`ReconcileSweep::note`], then call
    /// [`ReconcileSweep::finish_now`]. Holds the book's lock until finished.
    #[must_use]
    pub fn sweep(&self) -> ReconcileSweep<'_> {
        self.sweep_at(Instant::now())
    }

    fn sweep_at(&self, now: Instant) -> ReconcileSweep<'_> {
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
        }
    }

    /// The executor's report that a round on `socket` is over: the socket may
    /// plan again [`RECONCILE_ROUND_INTERVAL_SECS`] from now, and the
    /// instruments in `refund` (never actually resent) get their round back.
    fn round_finished(&self, socket: u8, refund: &[SubscribeInstrument], now: Instant) {
        let mut inner = self.lock();
        let Inner {
            sockets, expected, ..
        } = &mut *inner;
        bump_rounds(expected, refund, -1);
        if let Some(slot) = sockets.get_mut(usize::from(socket)) {
            slot.in_flight = false;
            slot.last_round_at = Some(now);
        }
    }
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
}

impl ReconcileSweep<'_> {
    /// Judges one silence report: O(1), one hash probe.
    pub fn note(&mut self, report: &SilenceReport) {
        let now = self.now;
        let inner = &mut *self.inner;
        let Some(entry) = inner.expected.get_mut(&report.key) else {
            return;
        };
        let Some(socket) = inner.sockets.get_mut(usize::from(entry.socket)) else {
            return;
        };
        match report.verdict {
            SilenceVerdict::NeverTicked => {}
            SilenceVerdict::Live | SilenceVerdict::Warming => {
                socket.delivering = socket.delivering.saturating_add(1);
                mark_recovered(entry);
                return;
            }
            // Ticked earlier, quiet now: neither delivering nor missing.
            SilenceVerdict::Exceeded => {
                mark_recovered(entry);
                return;
            }
        }
        if now.saturating_duration_since(entry.recorded_at)
            < Duration::from_secs(RECONCILE_DELTA_SECS)
        {
            return;
        }
        self.missing = self.missing.saturating_add(1);
        if entry.rounds >= RECONCILE_MAX_ROUNDS {
            // The last round gets its full 120 s after it finished before it
            // is judged spent.
            let last_round_settled = !socket.in_flight
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
            return;
        }
        if socket.candidates.len() < RECONCILE_MAX_PER_SOCKET_PER_ROUND {
            socket.candidates.push(entry.instrument);
        }
    }

    /// Ends the pass at the wall clock, with the trading-calendar session gate
    /// the frame-silence redials use.
    // WIRING-EXEMPT: called from the frame drain's 30 s silence arm, which another session owns; the one-line wiring is proposed in the PR body (plan item 3b).
    pub fn finish_now(self) -> SweepSummary {
        let secs = tickvault_core::websocket::silence_calendar::session_secs_of_day_now();
        self.finish_at(secs, ist_day_now())
    }

    /// Ends the pass at `secs_of_day_ist` on IST day `ist_day`.
    pub fn finish_at(mut self, secs_of_day_ist: u32, ist_day: i64) -> SweepSummary {
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
            for entry in inner.expected.values_mut() {
                entry.rounds = 0;
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
        if !FrameSilenceGate::ContinuousSessionIst.confirmed_window_open_at(secs_of_day_ist)
            || u64::from(secs_of_day_ist)
                < u64::from(CONTINUOUS_SESSION_OPEN_SECS_OF_DAY_IST)
                    .saturating_add(RECONCILE_DELTA_SECS)
        {
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
            sockets, expected, ..
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
            if socket.in_flight
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
                bump_rounds(expected, &instruments, 1);
                let round = PlannedRound {
                    tx: socket.tx.clone(),
                    socket: u8::try_from(index).unwrap_or(u8::MAX),
                    instruments,
                };
                match self.book.rounds_tx.try_send(round) {
                    Ok(()) => {
                        socket.in_flight = true;
                        summary.dispatched = summary.dispatched.saturating_add(1);
                        counter!(RECONCILE_ROUNDS_METRIC, "outcome" => "dispatched").increment(1);
                    }
                    Err(
                        mpsc::error::TrySendError::Full(round)
                        | mpsc::error::TrySendError::Closed(round),
                    ) => {
                        // Not a spent round: the instruments get theirs back
                        // and the socket keeps its spacing.
                        bump_rounds(expected, &round.instruments, -1);
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

/// Counts an instrument that ticked after a round was actually sent for it.
fn mark_recovered(entry: &mut Expected) {
    if entry.rounds > 0 && !entry.recovered_reported {
        entry.recovered_reported = true;
        counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "recovered").increment(1);
    }
}

/// Adds `delta` (+1 or -1) to each instrument's spent rounds. O(instruments).
fn bump_rounds(
    expected: &mut HashMap<Key, Expected>,
    instruments: &[SubscribeInstrument],
    delta: i8,
) {
    for instrument in instruments {
        if let Some(entry) = expected.get_mut(&key_of(instrument)) {
            entry.rounds = if delta >= 0 {
                entry.rounds.saturating_add(1).min(RECONCILE_MAX_ROUNDS)
            } else {
                entry.rounds.saturating_sub(1)
            };
        }
    }
}

/// Marks instruments as counted missing today; returns how many were new.
fn count_missing_once(
    expected: &mut HashMap<Key, Expected>,
    instruments: &[SubscribeInstrument],
) -> u64 {
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
    /// Every instrument that left the socket, and every one the socket did not
    /// hold, was subscribed again (or handed to its reconnect replay).
    Resubscribed {
        /// Instruments subscribed by the second command.
        resent: usize,
        /// Of those, instruments the socket did not hold at all.
        not_held: usize,
    },
    /// The socket refused every change after an 805; nothing changed.
    Refused {
        /// A `ResubscribeOutcome::REASON_*`.
        reason: &'static str,
    },
    /// The socket's task could not be reached; nothing changed.
    SendFailed,
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
    mut rounds: mpsc::Receiver<PlannedRound>,
) {
    while let Some(round) = rounds.recv().await {
        let Some(book) = book.upgrade() else {
            break;
        };
        let socket = round.socket;
        match execute_round(&book, round).await {
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

/// Sends one command and waits for the socket's answer. `None` when the
/// command could not be handed over; `Some(None)` when it was handed over and
/// no answer came back.
async fn send_and_wait(
    tx: &mpsc::Sender<LiveSubscriptionCommand>,
    unsubscribe: Vec<SubscribeInstrument>,
    subscribe: Vec<SubscribeInstrument>,
) -> Option<Option<ResubscribeOutcome>> {
    let (ack_tx, ack_rx) = oneshot::channel();
    let command = LiveSubscriptionCommand::Resubscribe {
        unsubscribe,
        subscribe,
        ack: Some(ack_tx),
    };
    tx.send_timeout(command, Duration::from_secs(RECONCILE_SEND_TIMEOUT_SECS))
        .await
        .ok()?;
    Some(
        tokio::time::timeout(Duration::from_secs(RECONCILE_ACK_TIMEOUT_SECS), ack_rx)
            .await
            .ok()
            .and_then(Result::ok),
    )
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
}

/// The unsubscribe half: removes the round's instruments from the socket,
/// splitting any batch the socket refuses as not held until each refused part
/// is one instrument. Bounded by [`RECONCILE_MAX_UNSUBSCRIBE_COMMANDS`].
async fn unsubscribe_half(
    tx: &mpsc::Sender<LiveSubscriptionCommand>,
    instruments: Vec<SubscribeInstrument>,
) -> UnsubscribeHalf {
    let count_round = |outcome: &'static str| {
        counter!(RECONCILE_ROUNDS_METRIC, "outcome" => outcome).increment(1);
    };
    let mut half = UnsubscribeHalf::default();
    let mut work: Vec<Vec<SubscribeInstrument>> = vec![instruments];
    let mut commands = 0usize;
    while let Some(batch) = work.pop() {
        if half.halted || commands >= RECONCILE_MAX_UNSUBSCRIBE_COMMANDS {
            if !half.halted {
                count_round("split_cap_reached");
            }
            half.untouched.extend(batch);
            continue;
        }
        commands += 1;
        match send_and_wait(tx, batch.clone(), Vec::new()).await {
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
/// and refunds the round for every instrument not actually resent.
pub async fn execute_round(book: &SubscribeReconcile, round: PlannedRound) -> RoundOutcome {
    let PlannedRound {
        tx,
        socket,
        instruments,
    } = round;
    execute_round_on(&tx, instruments, |refund| {
        book.round_finished(socket, refund, Instant::now());
    })
    .await
}

async fn execute_round_on<F>(
    tx: &mpsc::Sender<LiveSubscriptionCommand>,
    instruments: Vec<SubscribeInstrument>,
    finished: F,
) -> RoundOutcome
where
    F: FnOnce(&[SubscribeInstrument]),
{
    let count_round = |outcome: &'static str| {
        counter!(RECONCILE_ROUNDS_METRIC, "outcome" => outcome).increment(1);
    };
    let half = unsubscribe_half(tx, instruments).await;
    if half.unreachable && half.removed.is_empty() && half.unknown.is_empty() {
        count_round("send_failed");
        finished(&half.untouched);
        return RoundOutcome::SendFailed;
    }
    if half.halted && half.removed.is_empty() && half.unknown.is_empty() {
        count_round("refused_halted_805");
        finished(&half.untouched);
        return RoundOutcome::Refused {
            reason: ResubscribeOutcome::REASON_HALTED_805,
        };
    }
    let not_held = half.not_held.len();
    counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "not_held").increment(not_held as u64);
    let mut pending = half.removed;
    pending.extend(half.unknown);
    pending.extend(half.not_held);
    let resent = pending.len();
    if pending.is_empty() {
        finished(&half.untouched);
        return RoundOutcome::Resubscribed {
            resent: 0,
            not_held: 0,
        };
    }
    // Half two: put them back. Retried only on a definite failure; a missing
    // answer ends the round as unknown rather than resending blindly.
    for attempt in 0..RECONCILE_SUBSCRIBE_ATTEMPTS {
        if attempt > 0 {
            count_round("subscribe_retried");
            tokio::time::sleep(Duration::from_secs(RECONCILE_SUBSCRIBE_RETRY_SECS)).await;
        }
        match send_and_wait(tx, Vec::new(), pending.clone()).await {
            Some(Some(ResubscribeOutcome::Applied | ResubscribeOutcome::ReplayPending)) => {
                pending.clear();
                break;
            }
            Some(Some(ResubscribeOutcome::Stopped { not_subscribed, .. })) => {
                pending = not_subscribed;
                if pending.is_empty() {
                    break;
                }
            }
            Some(None) => {
                count_round("subscribe_unknown");
                counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "unknown")
                    .increment(pending.len() as u64);
                counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "resent")
                    .increment(resent.saturating_sub(pending.len()) as u64);
                finished(&half.untouched);
                return RoundOutcome::Unknown(pending);
            }
            // Refused (over cap: another change filled the room) or not handed
            // over: try again after the pause.
            Some(Some(ResubscribeOutcome::Refused { .. })) | None => {}
        }
    }
    counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "resent")
        .increment(resent.saturating_sub(pending.len()) as u64);
    finished(&half.untouched);
    if pending.is_empty() {
        count_round("resubscribed");
        RoundOutcome::Resubscribed { resent, not_held }
    } else {
        count_round("lost");
        counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "lost").increment(pending.len() as u64);
        RoundOutcome::Lost(pending)
    }
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
            book.round_finished(round.socket, &[], at);
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
        for _ in 0..5 {
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
        assert!(!inner.sockets[0].in_flight);
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
                if let Some(ack) = ack {
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
                not_held: 0
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
                not_held: 1
            }
        );
        assert!(refunded.is_empty(), "every instrument was resent");
        let (held, commands, duplicates) = handle.await.expect("guard socket");
        assert_eq!(held, sis(&[1, 2, 3, 4]), "all four are on the socket again");
        assert_eq!(duplicates, 0, "never re-offers a held instrument");
        assert!(commands <= 8, "the split is bounded: {commands} commands");
    }

    #[tokio::test]
    async fn test_execute_round_split_with_nothing_held_subscribes_everything() {
        let (tx, handle) = guard_socket(Vec::new());
        let (outcome, _) = run_round(tx, &[1, 2, 3]).await;
        assert_eq!(
            outcome,
            RoundOutcome::Resubscribed {
                resent: 3,
                not_held: 3
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
                not_held: 0
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
                not_held: 0
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
                not_held: 0
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
        assert!(book.lock().sockets[0].in_flight);
        let outcome = execute_round(&book, round).await;
        assert_eq!(
            outcome,
            RoundOutcome::Resubscribed {
                resent: 1,
                not_held: 0
            }
        );
        let inner = book.lock();
        assert!(!inner.sockets[0].in_flight, "the in-flight flag is cleared");
        assert!(inner.sockets[0].last_round_at.is_some());
        assert_eq!(inner.expected[&(2, ExchangeSegment::NseFno)].rounds, 1);
        assert_eq!(
            inner.expected[&(3, ExchangeSegment::NseFno)].rounds,
            0,
            "never resent, so refunded"
        );
        drop(inner);
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
        let executor = tokio::spawn(run_reconcile_executor(Arc::downgrade(&book), rounds_rx));
        book.rounds_tx
            .try_send(PlannedRound {
                tx,
                socket: 0,
                instruments: sis(&[7]),
            })
            .expect("room");
        // The round runs while the book lives; the socket's task ends once the
        // round drops its sender.
        let seen = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("the round ran")
            .expect("fake");
        assert_eq!(seen, vec![(sis(&[7]), vec![]), (vec![], sis(&[7]))]);
        // Dropping the only strong handle closes the queue: the loop ends.
        drop(book);
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
            })
            .expect("room");
        drop(book);
        run_reconcile_executor(std::sync::Weak::new(), rounds_rx).await;
        assert!(
            handle.await.expect("fake").is_empty(),
            "nothing sent for a gone lane"
        );
    }

    /// Cost the reconcile adds to the drain's 30 s silence scan at the
    /// authorized ceiling: 25,000 expected instruments, every report noted,
    /// one sweep finished. Wall clock, so `#[ignore]`d (never a CI gate):
    /// `cargo test -p tickvault-app --release --lib -- --ignored sweep_cost_at_the_authorized_ceiling --nocapture`.
    #[test]
    #[ignore = "wall-clock cost harness, run by hand in release; never a CI gate"]
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

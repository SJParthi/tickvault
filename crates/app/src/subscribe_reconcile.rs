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
//!   a close, never a redial for this (scope lock 2026-10-02). At most
//!   [`RECONCILE_MAX_ROUNDS`] rounds per instrument per IST day,
//!   [`RECONCILE_ROUND_INTERVAL_SECS`] apart per socket; then the instrument is
//!   counted as exhausted and named in one log line.
//! - **Switch.** `[dhan_universe] subscribe_reconcile_auto_resubscribe`,
//!   default `false`: the delta and the round planning run and are counted as
//!   `would_resubscribe`, and nothing is sent.
//!
//! ## Why two commands and not one
//!
//! `Resubscribe` treats an instrument named on BOTH lists as "stays held, costs
//! nothing on the wire", so one command cannot re-send a held instrument. The
//! first command removes them from the socket and from the reconnect replay
//! list; the second adds them back. The second is always sent (unless the
//! first was refused before anything changed), and it is retried, because
//! between the two the instruments are off the socket.
//!
//! ## Gates (each one keeps a whole-socket or whole-market silence out of it)
//!
//! - Continuous session only (09:15 to the confirmed-silence close, trading
//!   days, the same clock the frame-silence redials read).
//! - The whole feed must be delivering: if no expected instrument has ticked
//!   at all, it is a halt or a dead lane, not a partial subscribe.
//! - The socket must be delivering for at least one other expected
//!   instrument; a socket where nothing ticks is the frame-silence redial's
//!   job, counted here as `skipped_silent_socket` once per episode.
//!
//! ## Complexity
//!
//! `note` is one hash probe per silence report (O(1)), inside the drain's
//! existing O(n) 30 s sweep, never per tick. `finish` is O(sockets +
//! candidates). `record_subscribed` is O(instruments) once per subscribe
//! command, cold. Space is bounded: at most [`RECONCILE_MAX_EXPECTED`]
//! instruments (past it a new one is refused and counted) and
//! [`RECONCILE_MAX_SOCKETS`] sockets. The executor is O(instruments) per
//! round, on its own task.
//!
//! ## Honest limits
//!
//! - An instrument the silence detector does not track gives no report and is
//!   never judged.
//! - A contract that genuinely never trades can read as missing; it costs at
//!   most three resends a day and then one log line naming it.
//! - Between the two commands the instruments are off the socket (one
//!   round-trip, a few milliseconds); a process stop in that gap drops them
//!   from the reconnect replay until the next attach.
//! - Local `/metrics` only: no alarm, no page, no CloudWatch metric.

use std::collections::HashMap;
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

/// Minimum spacing between two rounds on the same socket. Matches the delta,
/// so a resent instrument gets the same 120 s to start before it is judged
/// again.
pub const RECONCILE_ROUND_INTERVAL_SECS: u64 = 120;

/// Most instruments one round resends on one socket (ten 100-instrument
/// messages each way). A larger delta is taken over several rounds.
pub const RECONCILE_MAX_PER_SOCKET_PER_ROUND: usize = 1_000;

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

/// Instruments named in one exhausted or lost log line.
const NAMED_IN_LOG: usize = 20;

/// Counter: one per round decision or executor result, by `outcome`.
pub const RECONCILE_ROUNDS_METRIC: &str = "tv_subscribe_reconcile_rounds_total";

/// Counter: instruments by what happened to them, by `outcome`.
pub const RECONCILE_INSTRUMENTS_METRIC: &str = "tv_subscribe_reconcile_instruments_total";

/// Gauge: expected instruments missing at the last sweep.
pub const RECONCILE_MISSING_GAUGE: &str = "tv_subscribe_reconcile_missing";

/// Every `outcome` label of [`RECONCILE_ROUNDS_METRIC`], registered at 0.
const ROUND_OUTCOMES: [&str; 13] = [
    "would_resubscribe",
    "dispatched",
    "queue_full",
    "skipped_silent_socket",
    "skipped_feed_silent",
    "resubscribed",
    "refused_not_held",
    "refused_over_cap",
    "refused_halted_805",
    "send_failed",
    "unsubscribe_unknown",
    "subscribe_retried",
    "lost",
];

/// Every `outcome` label of [`RECONCILE_INSTRUMENTS_METRIC`], registered at 0.
const INSTRUMENT_OUTCOMES: [&str; 6] = [
    "missing_found",
    "resent",
    "recovered",
    "exhausted",
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
    rounds: u8,
    exhausted_reported: bool,
    recovered_reported: bool,
}

/// One socket the lane subscribed on.
#[derive(Debug)]
struct Socket {
    tx: mpsc::Sender<LiveSubscriptionCommand>,
    last_round_at: Option<Instant>,
    ticking: u32,
    candidates: Vec<SubscribeInstrument>,
    silent_reported: bool,
}

#[derive(Debug)]
struct Inner {
    sockets: Vec<Socket>,
    expected: HashMap<Key, Expected>,
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
                    ticking: 0,
                    candidates: Vec::new(),
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
    /// [`ReconcileSweep::finish_now`]. Holds the book's lock until finished;
    /// the only other taker is a cold subscribe on an attach path.
    #[must_use]
    pub fn sweep(&self) -> ReconcileSweep<'_> {
        self.sweep_at(Instant::now())
    }

    fn sweep_at(&self, now: Instant) -> ReconcileSweep<'_> {
        let mut inner = self.lock();
        for socket in &mut inner.sockets {
            socket.ticking = 0;
            socket.candidates.clear();
        }
        ReconcileSweep {
            book: self,
            inner,
            now,
            missing: 0,
            exhausted: Vec::new(),
        }
    }

    /// Stops rounds for instruments a socket says it does not hold.
    fn mark_not_held(&self, instruments: &[SubscribeInstrument]) {
        let mut inner = self.lock();
        for instrument in instruments {
            if let Some(entry) = inner.expected.get_mut(&key_of(instrument)) {
                entry.rounds = RECONCILE_MAX_ROUNDS;
                entry.exhausted_reported = true;
            }
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
    exhausted: Vec<SubscribeInstrument>,
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
        if report.verdict != SilenceVerdict::NeverTicked {
            socket.ticking = socket.ticking.saturating_add(1);
            if entry.rounds > 0 && !entry.recovered_reported {
                entry.recovered_reported = true;
                counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "recovered").increment(1);
            }
            return;
        }
        if now.saturating_duration_since(entry.recorded_at)
            < Duration::from_secs(RECONCILE_DELTA_SECS)
        {
            return;
        }
        self.missing = self.missing.saturating_add(1);
        if entry.rounds >= RECONCILE_MAX_ROUNDS {
            // The last round gets its full 120 s before it is judged spent.
            let last_round_settled = socket.last_round_at.is_none_or(|at| {
                now.saturating_duration_since(at) >= Duration::from_secs(RECONCILE_DELTA_SECS)
            });
            if last_round_settled && !entry.exhausted_reported {
                entry.exhausted_reported = true;
                self.exhausted.push(entry.instrument);
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
            newly_exhausted: u32::try_from(self.exhausted.len()).unwrap_or(u32::MAX),
            ..SweepSummary::default()
        };
        gauge!(RECONCILE_MISSING_GAUGE).set(f64::from(self.missing));
        let inner = &mut *self.inner;
        if inner.ist_day != ist_day {
            // A new session: every instrument gets its rounds back.
            inner.ist_day = ist_day;
            inner.feed_silent_reported = false;
            for entry in inner.expected.values_mut() {
                entry.rounds = 0;
                entry.exhausted_reported = false;
                entry.recovered_reported = false;
            }
            for socket in &mut inner.sockets {
                socket.last_round_at = None;
                socket.silent_reported = false;
            }
            self.exhausted.clear();
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
        let feed_ticking: u64 = inner.sockets.iter().map(|s| u64::from(s.ticking)).sum();
        if feed_ticking == 0 {
            if summary.missing > 0 && !inner.feed_silent_reported {
                inner.feed_silent_reported = true;
                counter!(RECONCILE_ROUNDS_METRIC, "outcome" => "skipped_feed_silent").increment(1);
                warn!(
                    missing = summary.missing,
                    "subscribe reconcile: no subscribed instrument has sent a packet - this is a \
                     halt or a dead feed, not a partial subscribe; nothing is resent"
                );
            }
            return summary;
        }
        inner.feed_silent_reported = false;
        let interval = Duration::from_secs(RECONCILE_ROUND_INTERVAL_SECS);
        let Inner {
            sockets, expected, ..
        } = inner;
        for (index, socket) in sockets.iter_mut().enumerate() {
            if socket.candidates.is_empty() {
                socket.silent_reported = false;
                continue;
            }
            if socket.ticking == 0 {
                if !socket.silent_reported {
                    socket.silent_reported = true;
                    counter!(RECONCILE_ROUNDS_METRIC, "outcome" => "skipped_silent_socket")
                        .increment(1);
                    warn!(
                        socket = index,
                        missing = socket.candidates.len(),
                        "subscribe reconcile: a socket delivers nothing for any of its \
                         instruments - left to the frame-silence redial, nothing is resent"
                    );
                }
                continue;
            }
            socket.silent_reported = false;
            if socket
                .last_round_at
                .is_some_and(|at| self.now.saturating_duration_since(at) < interval)
            {
                continue;
            }
            let instruments = std::mem::take(&mut socket.candidates);
            let count = instruments.len() as u64;
            // One round spent per member, one hash probe each.
            bump_rounds(expected, &instruments, 1);
            if self.book.auto_resubscribe {
                let round = PlannedRound {
                    tx: socket.tx.clone(),
                    socket: u8::try_from(index).unwrap_or(u8::MAX),
                    instruments,
                };
                match self.book.rounds_tx.try_send(round) {
                    Ok(()) => {
                        summary.dispatched = summary.dispatched.saturating_add(1);
                        counter!(RECONCILE_ROUNDS_METRIC, "outcome" => "dispatched").increment(1);
                        counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "resent")
                            .increment(count);
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
                summary.would_resubscribe = summary.would_resubscribe.saturating_add(1);
                counter!(RECONCILE_ROUNDS_METRIC, "outcome" => "would_resubscribe").increment(1);
            }
            counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "missing_found").increment(count);
            socket.last_round_at = Some(self.now);
            info!(
                socket = index,
                instruments = count,
                auto_resubscribe = self.book.auto_resubscribe,
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
        if self.exhausted.is_empty() {
            return;
        }
        let n = self.exhausted.len();
        counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "exhausted").increment(n as u64);
        warn!(
            code = ErrorCode::WsGapSubscriptionBatching.code_str(),
            source = "subscribe_reconcile_exhausted",
            instruments = n,
            first = %NamedInstruments(&self.exhausted),
            "subscribe reconcile: instruments still sent nothing after the last resend round - \
             either they do not trade or the vendor is not serving them; not resent again today"
        );
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
    /// Both halves reached the socket (or its reconnect replay).
    Resubscribed,
    /// The unsubscribe half was refused before anything changed; the socket
    /// holds exactly what it held. `reason` is a `ResubscribeOutcome::REASON_*`.
    Refused {
        /// Why the socket refused.
        reason: &'static str,
    },
    /// The socket's task could not be reached; nothing changed.
    SendFailed,
    /// Instruments the unsubscribe half took off that the subscribe half could
    /// not put back after every attempt.
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
        let outcome = execute_round(&book, round).await;
        if let RoundOutcome::Lost(lost) = &outcome {
            error!(
                code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                source = "subscribe_reconcile_lost",
                socket,
                instruments = lost.len(),
                first = %NamedInstruments(lost),
                "subscribe reconcile: instruments were unsubscribed for a resend and could not \
                 be subscribed again - they are off this socket until the next attach or redial"
            );
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

/// Executes one round: unsubscribe the instruments, then subscribe them, on
/// the same socket. Cold, on the executor's task.
pub async fn execute_round(book: &SubscribeReconcile, round: PlannedRound) -> RoundOutcome {
    let count_round = |outcome: &'static str| {
        counter!(RECONCILE_ROUNDS_METRIC, "outcome" => outcome).increment(1);
    };
    // Half one: take them off. A refusal changed nothing, so the round ends.
    match send_and_wait(&round.tx, round.instruments.clone(), Vec::new()).await {
        None => {
            count_round("send_failed");
            return RoundOutcome::SendFailed;
        }
        Some(Some(ResubscribeOutcome::Refused { reason })) => {
            count_round(match reason {
                ResubscribeOutcome::REASON_NOT_HELD => "refused_not_held",
                ResubscribeOutcome::REASON_OVER_CAP => "refused_over_cap",
                _ => "refused_halted_805",
            });
            if reason == ResubscribeOutcome::REASON_NOT_HELD {
                // The book named the wrong socket: never try these again today.
                book.mark_not_held(&round.instruments);
            }
            return RoundOutcome::Refused { reason };
        }
        // Applied, Stopped part-way, ReplayPending: some or all left the
        // socket. No answer: unknown. In every case the subscribe half runs
        // for the whole list; one the socket still holds is dropped by the
        // socket's own duplicate check and costs no wire message.
        Some(None) => count_round("unsubscribe_unknown"),
        Some(Some(_)) => {}
    }
    // Half two: put them back, with retries, because they may now be off the
    // socket and off its reconnect replay.
    let mut pending = round.instruments;
    for attempt in 0..RECONCILE_SUBSCRIBE_ATTEMPTS {
        if attempt > 0 {
            count_round("subscribe_retried");
            tokio::time::sleep(Duration::from_secs(RECONCILE_SUBSCRIBE_RETRY_SECS)).await;
        }
        match send_and_wait(&round.tx, Vec::new(), pending.clone()).await {
            Some(Some(ResubscribeOutcome::Applied | ResubscribeOutcome::ReplayPending)) => {
                count_round("resubscribed");
                return RoundOutcome::Resubscribed;
            }
            Some(Some(ResubscribeOutcome::Stopped { not_subscribed, .. })) => {
                if not_subscribed.is_empty() {
                    count_round("resubscribed");
                    return RoundOutcome::Resubscribed;
                }
                pending = not_subscribed;
            }
            // Over cap means another change filled the room; retrying cannot
            // help, and the duplicate check makes a retry safe if it can.
            Some(Some(ResubscribeOutcome::Refused { .. })) | Some(None) | None => {}
        }
    }
    count_round("lost");
    counter!(RECONCILE_INSTRUMENTS_METRIC, "outcome" => "lost").increment(pending.len() as u64);
    RoundOutcome::Lost(pending)
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

    /// Book with socket A holding ids 1 (ticking) and 2 (missing), recorded at
    /// `t0`, then one sweep at `at`.
    fn one_sweep(
        book: &SubscribeReconcile,
        at: Instant,
        secs: u32,
        ticking: &[u64],
        missing: &[u64],
    ) -> SweepSummary {
        one_sweep_on(book, at, secs, DAY, ticking, missing)
    }

    fn one_sweep_on(
        book: &SubscribeReconcile,
        at: Instant,
        secs: u32,
        day: i64,
        ticking: &[u64],
        missing: &[u64],
    ) -> SweepSummary {
        let mut sweep = book.sweep_at(at);
        for id in ticking {
            sweep.note(&report(*id, SilenceVerdict::Live));
        }
        for id in missing {
            sweep.note(&report(*id, SilenceVerdict::NeverTicked));
        }
        sweep.finish_at(secs, day)
    }

    fn delta() -> Duration {
        Duration::from_secs(RECONCILE_DELTA_SECS)
    }

    #[test]
    fn test_sweep_finish_at_counts_missing_only_after_the_delta() {
        let (book, _rx) = SubscribeReconcile::new(false);
        let (tx, _sock) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &[si(1), si(2)], t0);
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
    fn test_shadow_mode_counts_and_sends_nothing() {
        let (book, mut rx) = SubscribeReconcile::new(false);
        let (tx, _sock) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &[si(1), si(2)], t0);
        let summary = one_sweep(&book, t0 + delta(), IN_SESSION, &[1], &[2]);
        assert_eq!(summary.would_resubscribe, 1);
        assert_eq!(summary.dispatched, 0);
        assert!(
            rx.try_recv().is_err(),
            "shadow mode must hand nothing to the executor"
        );
    }

    #[test]
    fn test_auto_mode_dispatches_the_missing_instruments_only() {
        let (book, mut rx) = SubscribeReconcile::new(true);
        let (tx, _sock) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &[si(1), si(2), si(3)], t0);
        let summary = one_sweep(&book, t0 + delta(), IN_SESSION, &[1], &[2, 3]);
        assert_eq!(summary.dispatched, 1);
        let round = rx.try_recv().expect("a round");
        assert_eq!(round.instruments, vec![si(2), si(3)]);
        assert!(
            round.tx.same_channel(&tx),
            "the round targets the holding socket"
        );
    }

    #[test]
    fn test_no_round_outside_the_session_or_in_its_first_120_s() {
        let (book, mut rx) = SubscribeReconcile::new(true);
        let (tx, _sock) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &[si(1), si(2)], t0);
        for secs in [8 * 3600, 9 * 3600 + 15 * 60, 9 * 3600 + 16 * 60, 16 * 3600] {
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
        book.record_subscribed_at(&tx_a, &[si(1)], t0);
        book.record_subscribed_at(&tx_b, &[si(2), si(3)], t0);
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
    fn test_a_silent_feed_is_a_halt_not_a_partial_subscribe() {
        let (book, mut rx) = SubscribeReconcile::new(true);
        let (tx, _sock) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &[si(1), si(2)], t0);
        let s = one_sweep(&book, t0 + delta(), IN_SESSION, &[], &[1, 2]);
        assert_eq!(s.missing, 2);
        assert_eq!(s.dispatched, 0);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_rounds_are_spaced_capped_and_then_exhausted_once() {
        let (book, mut rx) = SubscribeReconcile::new(true);
        let (tx, _sock) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &[si(1), si(2)], t0);
        let mut at = t0 + delta();
        let mut dispatched = 0;
        let mut exhausted = 0;
        // 30 s sweeps for 15 minutes.
        for _ in 0..30 {
            let s = one_sweep(&book, at, IN_SESSION, &[1], &[2]);
            dispatched += s.dispatched;
            exhausted += s.newly_exhausted;
            at += Duration::from_secs(30);
        }
        assert_eq!(
            dispatched,
            u32::from(RECONCILE_MAX_ROUNDS),
            "exactly max rounds, each at least 120 s apart"
        );
        let mut rounds = 0;
        while rx.try_recv().is_ok() {
            rounds += 1;
        }
        assert_eq!(rounds, usize::from(RECONCILE_MAX_ROUNDS));
        assert_eq!(exhausted, 1, "reported exhausted in exactly one sweep");
        let entry = book.lock().expected[&(2, ExchangeSegment::NseFno)];
        assert_eq!(entry.rounds, RECONCILE_MAX_ROUNDS);
        assert!(entry.exhausted_reported, "reported exhausted, once");
    }

    #[test]
    fn test_exhausted_waits_for_the_last_round_to_settle() {
        let (book, _rx) = SubscribeReconcile::new(true);
        let (tx, _sock) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &[si(1), si(2)], t0);
        let mut at = t0 + delta();
        for _ in 0..RECONCILE_MAX_ROUNDS {
            one_sweep(&book, at, IN_SESSION, &[1], &[2]);
            at += delta();
        }
        // The third round ran at `at - delta`; 30 s later is too soon.
        let soon = at - delta() + Duration::from_secs(30);
        one_sweep(&book, soon, IN_SESSION, &[1], &[2]);
        assert!(!book.lock().expected[&(2, ExchangeSegment::NseFno)].exhausted_reported);
        one_sweep(&book, at, IN_SESSION, &[1], &[2]);
        assert!(book.lock().expected[&(2, ExchangeSegment::NseFno)].exhausted_reported);
    }

    #[test]
    fn test_a_new_ist_day_gives_rounds_back() {
        let (book, mut rx) = SubscribeReconcile::new(true);
        let (tx, _sock) = socket();
        let t0 = Instant::now();
        book.record_subscribed_at(&tx, &[si(1), si(2)], t0);
        let mut at = t0 + delta();
        for _ in 0..5 {
            one_sweep(&book, at, IN_SESSION, &[1], &[2]);
            at += delta();
        }
        let mut spent = 0;
        while rx.try_recv().is_ok() {
            spent += 1;
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
        // From the next sweep the instrument has its rounds back.
        let s = one_sweep_on(&book, at + delta(), IN_SESSION, DAY + 1, &[1], &[2]);
        assert_eq!(s.dispatched, 1);
        assert_eq!(
            book.lock().expected[&(2, ExchangeSegment::NseFno)].rounds,
            1
        );
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
        book.record_subscribed_at(&tx, &[si(1), si(2)], t0);
        let s = one_sweep(&book, t0 + delta(), IN_SESSION, &[1], &[2]);
        assert_eq!(s.dispatched, 0);
        let inner = book.lock();
        assert_eq!(inner.expected[&(2, ExchangeSegment::NseFno)].rounds, 0);
        assert!(
            inner.sockets[0].last_round_at.is_none(),
            "spacing not consumed"
        );
    }

    #[test]
    fn test_record_subscribed_keeps_the_first_socket_and_dedups() {
        let (book, _rx) = SubscribeReconcile::new(false);
        let (tx_a, _a) = socket();
        let (tx_b, _b) = socket();
        book.record_subscribed(&tx_a, &[si(1), si(1)]);
        book.record_subscribed(&tx_b, &[si(1), si(2)]);
        assert_eq!(book.expected_len(), 2);
        let inner = book.lock();
        assert_eq!(inner.expected[&(1, ExchangeSegment::NseFno)].socket, 0);
        assert_eq!(inner.expected[&(2, ExchangeSegment::NseFno)].socket, 1);
        assert_eq!(inner.sockets.len(), 2);
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
        book.record_subscribed_at(&tx, &[si(1), si(2)], t0);
        book.forget(&[si(2)]);
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
        let few = [si(1), si(2)];
        assert_eq!(NamedInstruments(&few).to_string(), "NSE_FNO:1,NSE_FNO:2");
        let many: Vec<SubscribeInstrument> = (0..25).map(si).collect();
        let text = NamedInstruments(&many).to_string();
        assert!(text.ends_with(",+5 more"), "{text}");
        assert_eq!(NamedInstruments(&[]).to_string(), "");
    }

    #[test]
    fn test_finish_now_reads_the_wall_clock_without_panicking() {
        // Whatever the wall clock says, finish_now must classify the pass and
        // return; outside the session it dispatches nothing.
        let (book, _rx) = SubscribeReconcile::new(false);
        let (tx, _s) = socket();
        book.record_subscribed(&tx, &[si(1)]);
        let mut sweep = book.sweep();
        sweep.note(&report(1, SilenceVerdict::Live));
        let summary = sweep.finish_now();
        assert_eq!(summary.dispatched, 0, "shadow mode never dispatches");
    }

    #[test]
    fn test_ist_day_now_is_a_plausible_day() {
        // 2026-01-01 is day 20,454 since the Unix epoch.
        assert!(ist_day_now() > 20_454);
    }

    // ---- executor ---------------------------------------------------------

    /// A fake connection task: answers each command with the next scripted
    /// outcome (`None` = drop the ack unanswered) and records what it got.
    fn fake_socket(
        script: Vec<Option<ResubscribeOutcome>>,
    ) -> (
        mpsc::Sender<LiveSubscriptionCommand>,
        tokio::task::JoinHandle<Vec<(Vec<SubscribeInstrument>, Vec<SubscribeInstrument>)>>,
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

    fn round(tx: mpsc::Sender<LiveSubscriptionCommand>, ids: &[u64]) -> PlannedRound {
        PlannedRound {
            tx,
            socket: 0,
            instruments: ids.iter().copied().map(si).collect(),
        }
    }

    #[tokio::test]
    async fn test_execute_round_unsubscribes_then_subscribes_on_the_same_socket() {
        let (book, _rx) = SubscribeReconcile::new(true);
        let (tx, handle) = fake_socket(vec![
            Some(ResubscribeOutcome::Applied),
            Some(ResubscribeOutcome::Applied),
        ]);
        let outcome = execute_round(&book, round(tx, &[5, 6])).await;
        assert_eq!(outcome, RoundOutcome::Resubscribed);
        let seen = handle.await.expect("fake socket");
        assert_eq!(seen.len(), 2);
        assert_eq!(
            seen[0],
            (vec![si(5), si(6)], vec![]),
            "unsubscribe only, first"
        );
        assert_eq!(
            seen[1],
            (vec![], vec![si(5), si(6)]),
            "subscribe only, second"
        );
    }

    #[tokio::test]
    async fn test_execute_round_refused_unsubscribe_sends_nothing_more() {
        let (book, _rx) = SubscribeReconcile::new(true);
        let (tx_rec, _s) = socket();
        book.record_subscribed(&tx_rec, &[si(5)]);
        let (tx, handle) = fake_socket(vec![Some(ResubscribeOutcome::Refused {
            reason: ResubscribeOutcome::REASON_NOT_HELD,
        })]);
        let outcome = execute_round(&book, round(tx, &[5])).await;
        assert_eq!(
            outcome,
            RoundOutcome::Refused {
                reason: ResubscribeOutcome::REASON_NOT_HELD
            }
        );
        assert_eq!(
            handle.await.expect("fake").len(),
            1,
            "no subscribe after a refusal"
        );
        let entry = book.lock().expected[&(5, ExchangeSegment::NseFno)];
        assert_eq!(
            entry.rounds, RECONCILE_MAX_ROUNDS,
            "never tried again today"
        );
    }

    #[tokio::test]
    async fn test_execute_round_unreachable_socket_is_send_failed() {
        let (book, _rx) = SubscribeReconcile::new(true);
        let (tx, rx) = socket();
        drop(rx);
        assert_eq!(
            execute_round(&book, round(tx, &[5])).await,
            RoundOutcome::SendFailed
        );
    }

    #[tokio::test]
    async fn test_execute_round_retries_the_part_not_subscribed() {
        let (book, _rx) = SubscribeReconcile::new(true);
        let (tx, handle) = fake_socket(vec![
            Some(ResubscribeOutcome::ReplayPending),
            Some(ResubscribeOutcome::Stopped {
                not_unsubscribed: vec![],
                not_subscribed: vec![si(6)],
            }),
            Some(ResubscribeOutcome::Applied),
        ]);
        tokio::time::pause();
        let outcome = execute_round(&book, round(tx, &[5, 6])).await;
        assert_eq!(outcome, RoundOutcome::Resubscribed);
        let seen = handle.await.expect("fake");
        assert_eq!(seen.len(), 3);
        assert_eq!(
            seen[2],
            (vec![], vec![si(6)]),
            "only the unsent part is retried"
        );
    }

    #[tokio::test]
    async fn test_execute_round_unanswered_unsubscribe_still_resubscribes() {
        let (book, _rx) = SubscribeReconcile::new(true);
        // First ack dropped unanswered: the outcome is unknown, so the
        // subscribe half still runs (a held duplicate costs no wire message).
        let (tx, handle) = fake_socket(vec![None, Some(ResubscribeOutcome::Applied)]);
        let outcome = execute_round(&book, round(tx, &[5])).await;
        assert_eq!(outcome, RoundOutcome::Resubscribed);
        assert_eq!(handle.await.expect("fake").len(), 2);
    }

    #[tokio::test]
    async fn test_execute_round_reports_lost_after_every_attempt_fails() {
        let (book, _rx) = SubscribeReconcile::new(true);
        let mut script = vec![Some(ResubscribeOutcome::Applied)];
        for _ in 0..RECONCILE_SUBSCRIBE_ATTEMPTS {
            script.push(Some(ResubscribeOutcome::Refused {
                reason: ResubscribeOutcome::REASON_OVER_CAP,
            }));
        }
        let (tx, handle) = fake_socket(script);
        tokio::time::pause();
        let outcome = execute_round(&book, round(tx, &[5, 6])).await;
        assert_eq!(outcome, RoundOutcome::Lost(vec![si(5), si(6)]));
        assert_eq!(
            handle.await.expect("fake").len(),
            1 + RECONCILE_SUBSCRIBE_ATTEMPTS as usize,
            "bounded: one unsubscribe and at most the attempt cap of subscribes"
        );
    }

    #[tokio::test]
    async fn test_run_reconcile_executor_runs_rounds_until_the_book_is_dropped() {
        let (book, rounds_rx) = SubscribeReconcile::new(true);
        let (tx, handle) = fake_socket(vec![
            Some(ResubscribeOutcome::Applied),
            Some(ResubscribeOutcome::Applied),
        ]);
        let executor = tokio::spawn(run_reconcile_executor(Arc::downgrade(&book), rounds_rx));
        book.rounds_tx.try_send(round(tx, &[7])).expect("room");
        // The round runs while the book lives; the socket's task ends once the
        // round drops its sender.
        let seen = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("the round ran")
            .expect("fake");
        assert_eq!(seen, vec![(vec![si(7)], vec![]), (vec![], vec![si(7)])]);
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
        book.rounds_tx.try_send(round(tx, &[7])).expect("room");
        drop(book);
        run_reconcile_executor(std::sync::Weak::new(), rounds_rx).await;
        assert!(
            handle.await.expect("fake").is_empty(),
            "nothing sent for a gone lane"
        );
    }
}

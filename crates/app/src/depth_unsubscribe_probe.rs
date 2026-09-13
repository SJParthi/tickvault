//! The operator-armed unsubscribe probe — two arms, one question each.
//!
//! # Why this exists
//!
//! Dhan has ignored our depth unsubscribe on two consecutive sessions, with
//! two different request codes. 2026-09-10 shipped code 25 and measured
//! 110,114 ghost packets; 2026-09-11 shipped code 24 and measured 5,345,436.
//! Neither is "honoured" and neither is "ignored" outright: the
//! traffic-independent `ghost / unsubscribed_grace` ratio read 1.996 and
//! 3.170 against a never-honoured ceiling of 4.667, so some unsubscribes are
//! taking effect and some are not.
//!
//! Both of those numbers came from ~9,000 swaps a session, planned by the
//! ranked steering, against a background of 24x-tick-volume depth traffic.
//! **That is the problem this module solves.** A finding built on confounded
//! aggregate counters invites "we cannot reproduce it — send a capture". A
//! finding built on ONE named contract, dropped at a known instant, on a
//! socket with a measured baseline, does not.
//!
//! # The two arms, and why one is not enough
//!
//! The operator's own words settled the shape, and he was right:
//!
//! > "Why bro youdidnf include so key disconnect and reconnect dude because
//! > of we don't check both of them then nowhere we will easily identify
//! > which one is working right dude"
//!
//! | | Arm A — unsubscribe | Arm B — socket close |
//! |---|---|---|
//! | Mechanism | `RequestCode` 25 on a live socket | close, re-dial, replay a set WITHOUT the contract |
//! | Answers | does the vendor honour an unsubscribe? | can we stop a stream at all? |
//! | If it fails | the vendor ignores the request | our redial/replay path is broken, not the vendor |
//!
//! Running only Arm A gives a verdict with no control: silence could mean the
//! vendor honoured it OR that the book went quiet, and continued frames could
//! mean the vendor ignored it OR that our own replay put it back. Running
//! both, on the same day, on the same pool, separates the vendor's behaviour
//! from ours — which is the entire point.
//!
//! # Admissibility, which is most of the code
//!
//! The India feed has **no snapshot-on-subscribe**, so a contract that stops
//! arriving may simply not be trading. Silence is therefore evidence ONLY
//! when the contract was demonstrably arriving beforehand. Every run takes a
//! BASELINE first and refuses to touch the wire without one — a thin book
//! reports [`ProbeVerdict::InconclusiveThinBook`] and nothing is dropped.
//!
//! # What this module does NOT do
//!
//! It does not email Dhan and it does not post to MadeForTrade. The operator
//! asked for both; the scope lock records that auto-send is not authorized,
//! and the house workflow requires every support mail to be a committed
//! markdown file shared as a rendered GitHub link, reviewed by a human. So
//! the probe produces the EVIDENCE — one coded line carrying the contract,
//! the request code, both counts, the window and the connection index — and
//! the human writes the ticket from it.
//!
//! # Complexity
//!
//! Cold path, once per session, behind a default-OFF config flag. The watch
//! costs nothing per packet: it reuses [`crate::depth_first_packet`], whose
//! hot-path arm is one relaxed atomic load when no watch is armed.

use std::time::Duration;

use tickvault_common::config::DepthUnsubscribeProbeConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_core::parser::depth::DepthFeedKind;
use tickvault_core::websocket::pool_supervisor::{
    LiveSubscriptionCommand, ProbeUnsubscribeOutcome, SubscribeInstrument, dial_generation,
    request_probe_close,
};
use tracing::{error, info, warn};

use crate::depth_rebalance::RebalanceSocket;

/// Counter: one per completed probe run. Label: `arm`, `verdict`.
///
/// In-process only — deliberately not EMF-selected and deliberately not
/// alarmed. At most two increments per session, read from the log line beside
/// it; a CloudWatch series would cost ~$0.30/mo against a September forecast
/// of $142.24 with the automatic `STOP_EC2_INSTANCES` line at $135.00, and
/// the noise lock's §2.3n rule wants a LEVER for a new series, not a cost
/// note.
pub const PROBE_RUN_METRIC: &str = "tv_depth_unsubscribe_probe_total";

/// How long the probe watches for frames after the action.
///
/// Deliberately SHORTER than [`crate::depth_subscription_view::GHOST_GRACE_SECS`],
/// and the const-assert below is what keeps it that way. The ghost detector
/// classifies "dropped by us, still arriving, past the grace" as a ghost and
/// schedules a re-dial for it — which is Arm B's mechanism. A watch window
/// that outlived the grace would let the ghost detector re-dial Arm A's
/// socket mid-measurement and silently apply Arm B's mechanism to Arm A's
/// question. A flag would drift; an assert fails the build.
pub const PROBE_WATCH_SECS: u64 = 60;

/// How long the probe watches BEFORE the action, to prove the book is live.
///
/// Without this, silence after the drop is unreadable: the India feed sends
/// no snapshot on subscribe, so a contract that goes quiet may simply not be
/// trading. A baseline makes the verdict admissible, and its absence makes
/// the run refuse rather than guess.
pub const PROBE_BASELINE_SECS: u64 = 60;

const _: () = assert!(
    (PROBE_WATCH_SECS as i64) < crate::depth_subscription_view::GHOST_GRACE_SECS,
    "the probe's watch window must end BEFORE the ghost detector's grace, or the ghost \
     re-dial applies Arm B's mechanism to Arm A's measurement and the verdict is the \
     other arm's answer wearing this arm's label"
);

/// Which mechanism a run exercises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeArm {
    /// Send `RequestCode` 25 for one contract and put nothing back.
    Unsubscribe,
    /// Close the socket and let the replay come back without the contract.
    SocketClose,
}

impl ProbeArm {
    /// The metric label and the log field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unsubscribe => "unsubscribe",
            Self::SocketClose => "socket_close",
        }
    }
}

/// What one run concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeVerdict {
    /// Frames STOPPED. The mechanism worked for this contract, this once.
    Honoured,
    /// Frames KEPT ARRIVING for a contract this process had dropped. For Arm
    /// A that is the vendor ignoring `RequestCode` 25; for Arm B it means the
    /// stream survived a full close and re-dial, which would be a far
    /// stranger finding and worth a ticket on its own.
    Ignored,
    /// The contract sent NOTHING during the baseline, so nothing it does
    /// afterwards is evidence. Nothing was dropped and nothing was restored.
    InconclusiveThinBook,
    /// The command was refused before reaching the wire — the socket did not
    /// hold exactly the contract the probe named. Usually the ranked steering
    /// swapping the strike between selection and dispatch.
    InconclusiveRefused,
    /// The frame did not reach the wire, or the channel would not take the
    /// command. Nothing changed.
    InconclusiveWireFailed,
    /// No depth-200 socket was carrying a contract to probe.
    InconclusiveNoTarget,
    /// The connection RE-DIALED during Arm A's watch, so the silence — if any
    /// — is ours and not the vendor's.
    ///
    /// Arm A empties the guard, and the guard IS the reconnect replay, so a
    /// socket that came back during the window came back subscribed to
    /// NOTHING. Frames would stop for a reason that says nothing at all about
    /// request code 25. Before 2026-09-13 this produced a confident
    /// `honoured` and a hand-written warning in the log asking the reader to
    /// check `ws_event_audit` themselves.
    InconclusiveRedialled,
    /// Arm B closed the socket and it did NOT come back inside the watch, so
    /// silence proves nothing.
    ///
    /// A slot still climbing its backoff ladder delivers no frames for a
    /// reason that has nothing to do with the replay set. Without this the
    /// arm would report `honoured` for a socket that was simply down —
    /// exactly the confound the baseline exists to prevent, arriving from the
    /// other end of the run.
    InconclusiveNotRedialled,
    /// The first-packet tracker REFUSED the watch stamp, so no window was
    /// ever armed and nothing was measured.
    ///
    /// The tracker declines when its pending map is at `MAX_PENDING`. A
    /// refusal used to be indistinguishable from "a frame arrived" — no entry
    /// exists, so the "is it still pending?" read comes back empty — which
    /// turned an unstarted measurement into a vendor-blaming `ignored`.
    InconclusiveNotMeasured,
}

impl ProbeVerdict {
    /// The metric label and the log field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Honoured => "honoured",
            Self::Ignored => "ignored",
            Self::InconclusiveThinBook => "inconclusive_thin_book",
            Self::InconclusiveRefused => "inconclusive_refused",
            Self::InconclusiveWireFailed => "inconclusive_wire_failed",
            Self::InconclusiveNoTarget => "inconclusive_no_target",
            Self::InconclusiveRedialled => "inconclusive_redialled",
            Self::InconclusiveNotRedialled => "inconclusive_not_redialled",
            Self::InconclusiveNotMeasured => "inconclusive_not_measured",
        }
    }

    /// Whether this verdict says anything about the vendor at all.
    ///
    /// Only two do. Everything else is the probe declining to answer, and a
    /// support ticket built on one of them would be built on nothing.
    #[must_use]
    pub const fn is_conclusive(self) -> bool {
        matches!(self, Self::Honoured | Self::Ignored)
    }
}

/// Reads "did a frame arrive for this contract during the window" from the
/// first-packet tracker.
///
/// # The polarity inversion, stated plainly
///
/// [`crate::depth_first_packet`] exists to answer "how long until the FIRST
/// packet after a subscribe". This asks the opposite question with the same
/// machinery: arm a watch, and if it is STILL PENDING when the window closes,
/// nothing arrived.
///
/// Reused rather than reimplemented because the alternative is a second
/// per-packet observation point on the depth decode path — a hot-path change,
/// with its own DHAT gate, to serve a measurement that runs twice a session.
/// The tracker's hot arm is one relaxed atomic load when nothing is armed, so
/// this costs the steady state nothing at all.
///
/// The `may_already_be_streaming` gate is passed `false` DELIBERATELY. That
/// gate exists to stop a ghost from answering a fresh subscribe stamp with a
/// fabricated ~0 ms. Here "is it still streaming?" is the entire question, so
/// honouring the gate would refuse every probe by construction.
async fn any_frame_within(
    instrument: SubscribeInstrument,
    window: Duration,
    started_nanos: i64,
) -> FrameWatch {
    let tracker = crate::depth_first_packet::global_depth_first_packet_tracker();
    // A REFUSED stamp arms no window. Reading the refusal as "nothing was
    // pending, so a frame must have arrived" is how an unstarted measurement
    // became a verdict; the arm has to be distinguishable, not merely rare.
    if !tracker.record_subscribe_at(
        instrument.security_id,
        instrument.segment,
        DepthFeedKind::TwoHundred,
        started_nanos,
        false,
    ) {
        return FrameWatch::NotMeasured;
    }
    tokio::time::sleep(window).await;
    // `forget` returns true when the entry was STILL THERE — i.e. no packet
    // resolved it. The window is shorter than the tracker's own sweep, so a
    // `false` here can only mean a packet arrived, never that it aged out.
    if tracker.forget(
        instrument.security_id,
        instrument.segment,
        DepthFeedKind::TwoHundred,
    ) {
        FrameWatch::Silent
    } else {
        FrameWatch::Arrived
    }
}

/// What one watch window observed. Three states, not two, because "we never
/// armed a window" is a different fact from "the window was quiet".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameWatch {
    /// At least one depth frame resolved the watch.
    Arrived,
    /// The window closed with the stamp still pending — nothing arrived.
    Silent,
    /// No window was armed; the tracker refused the stamp.
    NotMeasured,
}

/// Picks the depth-200 socket to probe: the FIRST one holding a contract.
///
/// Pure and separate so the choice is testable without a socket. Returns the
/// index into `sockets`, never the connection index — the caller reads that
/// from the socket it selected, because the two are not the same number and
/// confusing them tears down the wrong connection.
#[must_use]
pub fn pick_target(sockets: &[RebalanceSocket], skip: &[usize]) -> Option<usize> {
    sockets
        .iter()
        .enumerate()
        .find(|(index, socket)| !skip.contains(index) && socket.held.is_some())
        .map(|(index, _)| index)
}

/// Runs ONE arm against ONE socket, start to finish, and reports the verdict.
///
/// Blocks its caller for roughly `PROBE_BASELINE_SECS + PROBE_WATCH_SECS`.
/// That is deliberate and it has a real cost: the steering loop this runs on
/// re-centres at-the-money windows every minute, so an armed probe costs one
/// or two minutes of stale strikes, once per session. The alternative — a
/// spawned task holding a cloned sender — cannot keep the steering from
/// swapping the socket underneath the measurement, which would contaminate
/// the one thing the probe exists to produce.
pub async fn run_arm(socket: &mut RebalanceSocket, arm: ProbeArm) -> ProbeVerdict {
    let Some(instrument) = socket.held else {
        return ProbeVerdict::InconclusiveNoTarget;
    };
    let connection_index = socket.connection_index;
    info!(
        source = "probe_baseline_started",
        arm = arm.as_str(),
        connection_index,
        security_id = instrument.security_id,
        segment = instrument.segment.as_str(),
        baseline_secs = PROBE_BASELINE_SECS,
        "unsubscribe probe: watching this contract BEFORE touching it, because silence \
         afterwards is only evidence if the book was live beforehand"
    );
    let now_nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    match any_frame_within(
        instrument,
        Duration::from_secs(PROBE_BASELINE_SECS),
        now_nanos,
    )
    .await
    {
        FrameWatch::Arrived => {}
        FrameWatch::Silent => {
            return finish(
                arm,
                connection_index,
                instrument,
                ProbeVerdict::InconclusiveThinBook,
            );
        }
        FrameWatch::NotMeasured => {
            return finish(
                arm,
                connection_index,
                instrument,
                ProbeVerdict::InconclusiveNotMeasured,
            );
        }
    }

    // Captured AFTER the baseline and BEFORE the action, so the comparison
    // covers exactly the interval whose meaning a re-dial would change.
    let dials_before = dial_generation(connection_index);

    // BOTH arms empty the guard. Arm A does it with a frame on the wire —
    // that IS its question. Arm B does it silently, because the guard is the
    // reconnect replay and its question is whether a replayed set WITHOUT the
    // contract stops the stream. An Arm B that left the guard alone re-dialed
    // straight back into the same subscription, so frames always resumed and
    // the shared mapping below returned `ignored` whatever the vendor did.
    let acted = match arm {
        ProbeArm::Unsubscribe => act_drop(socket, instrument, true).await,
        ProbeArm::SocketClose => match act_drop(socket, instrument, false).await {
            Ok(()) => act_socket_close(connection_index),
            Err(verdict) => Err(verdict),
        },
    };
    if let Err(verdict) = acted {
        return finish(arm, connection_index, instrument, verdict);
    }

    let watch_started = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let watched = any_frame_within(
        instrument,
        Duration::from_secs(PROBE_WATCH_SECS),
        watch_started,
    )
    .await;
    let dials_after = dial_generation(connection_index);

    // Both arms emptied the guard, so both must put the contract back. This
    // runs BEFORE the verdict is computed on purpose: leaving a depth-200
    // socket dark is a capture loss, and it must not wait on a branch.
    restore(socket, instrument).await;

    let verdict = verdict_for(arm, watched, dials_before, dials_after);
    finish(arm, connection_index, instrument, verdict)
}

/// Turns one watch result plus the re-dial witness into a verdict. Pure, so
/// every branch is testable without a socket.
///
/// The re-dial check runs in OPPOSITE directions for the two arms, which is
/// the whole reason one counter serves both: Arm A is invalidated BY a
/// re-dial (its emptied guard replays nothing, so the silence is ours), and
/// Arm B is invalidated by the ABSENCE of one (a socket still climbing its
/// backoff ladder is silent for reasons that are not the vendor's).
#[must_use]
fn verdict_for(
    arm: ProbeArm,
    watched: FrameWatch,
    dials_before: u64,
    dials_after: u64,
) -> ProbeVerdict {
    let redialled = dials_after > dials_before;
    match (arm, watched) {
        (_, FrameWatch::NotMeasured) => ProbeVerdict::InconclusiveNotMeasured,
        // Frames arriving is conclusive for BOTH arms and needs no witness:
        // whatever the socket did, the vendor kept delivering an instrument
        // this process had removed from its subscription set.
        (_, FrameWatch::Arrived) => ProbeVerdict::Ignored,
        (ProbeArm::Unsubscribe, FrameWatch::Silent) if redialled => {
            ProbeVerdict::InconclusiveRedialled
        }
        (ProbeArm::SocketClose, FrameWatch::Silent) if !redialled => {
            ProbeVerdict::InconclusiveNotRedialled
        }
        (_, FrameWatch::Silent) => ProbeVerdict::Honoured,
    }
}

/// Removes the contract from this socket's subscription set and puts nothing
/// back, with (`send_wire` true, Arm A) or without (false, Arm B) an
/// unsubscribe frame.
///
/// Both arms need the guard emptied — the guard is what a re-dial replays —
/// and they differ only in whether the vendor is told. Sharing one function
/// is deliberate: the fail-closed "this socket must hold EXACTLY the named
/// contract" check lives in the handler, and an arm with its own path would
/// be an arm that could skip it.
async fn act_drop(
    socket: &mut RebalanceSocket,
    instrument: SubscribeInstrument,
    send_wire: bool,
) -> Result<(), ProbeVerdict> {
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    if socket
        .tx
        .try_send(LiveSubscriptionCommand::ProbeUnsubscribe {
            drop_this: instrument,
            send_wire,
            ack: Some(ack_tx),
        })
        .is_err()
    {
        return Err(ProbeVerdict::InconclusiveWireFailed);
    }
    // Bounded: the connection answers as soon as it drains the command, and a
    // task that cannot answer inside the wait is one whose verdict would be
    // meaningless anyway. Never unbounded — this runs on the steering loop.
    match tokio::time::timeout(Duration::from_secs(ACK_WAIT_SECS), ack_rx).await {
        Ok(Ok(ProbeUnsubscribeOutcome::Dropped)) => {
            // Believed-held follows the WIRE here, unlike a swap, because
            // nothing replaced it: leaving `held` naming the contract would
            // have the next steering minute plan a swap from an instrument
            // the socket no longer carries, which the guard refuses.
            socket.held = None;
            Ok(())
        }
        Ok(Ok(ProbeUnsubscribeOutcome::Refused { .. })) => Err(ProbeVerdict::InconclusiveRefused),
        Ok(Ok(ProbeUnsubscribeOutcome::WireFailed { .. })) | Ok(Err(_)) | Err(_) => {
            Err(ProbeVerdict::InconclusiveWireFailed)
        }
    }
}

/// Arm B, second half: close the socket and let the ladder bring it back.
///
/// Called only AFTER [`act_drop`] has emptied the guard, so the replay comes
/// back WITHOUT the contract. That ordering is the arm: a close on its own
/// re-subscribes the same instrument and measures nothing.
///
/// What this asks that Arm A cannot: whether a stream can be stopped at all,
/// by any mechanism we own. Frames arriving afterwards mean the vendor kept
/// delivering to a connection that never asked for the instrument — a far
/// stronger finding than an ignored request code, and one a support ticket
/// can be built on without a single inference.
fn act_socket_close(connection_index: u8) -> Result<(), ProbeVerdict> {
    match request_probe_close(connection_index) {
        Ok(()) => Ok(()),
        Err(refusal) => {
            warn!(
                code = ErrorCode::WsGapConnectionState.code_str(),
                source = "probe_close_refused",
                connection_index,
                reason = refusal.as_str(),
                "unsubscribe probe Arm B could not arm a close for this socket"
            );
            Err(ProbeVerdict::InconclusiveWireFailed)
        }
    }
}

/// Puts Arm A's contract back on the socket it was taken from.
///
/// A failure here is LOUD and is not folded into the verdict: the measurement
/// already happened, and what is at stake now is a depth-200 socket left
/// carrying nothing for the rest of the session. That is a capture loss, not
/// an inconclusive reading, and it must not be reported as one.
async fn restore(socket: &mut RebalanceSocket, instrument: SubscribeInstrument) {
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    if socket
        .tx
        .try_send(LiveSubscriptionCommand::Extend {
            more: vec![instrument],
            ack: Some(ack_tx),
        })
        .is_err()
    {
        error!(
            code = ErrorCode::WsGapSubscriptionBatching.code_str(),
            source = "probe_restore_failed",
            connection_index = socket.connection_index,
            security_id = instrument.security_id,
            "unsubscribe probe could not queue its RESTORE — this depth-200 socket is \
             carrying NOTHING and will keep ponging while delivering no data. The next \
             at-the-money move re-subscribes it; until then this socket is dark."
        );
        return;
    }
    match tokio::time::timeout(Duration::from_secs(ACK_WAIT_SECS), ack_rx).await {
        Ok(Ok(tickvault_core::websocket::pool_supervisor::ExtendOutcome::Held)) => {
            socket.held = Some(instrument);
            info!(
                source = "probe_restore_ok",
                connection_index = socket.connection_index,
                security_id = instrument.security_id,
                "unsubscribe probe restored the contract it borrowed"
            );
        }
        other => {
            error!(
                code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                source = "probe_restore_failed",
                connection_index = socket.connection_index,
                security_id = instrument.security_id,
                outcome = ?other,
                "unsubscribe probe's RESTORE did not take — this depth-200 socket is \
                 carrying NOTHING until the next at-the-money move"
            );
        }
    }
}

/// How long to wait for a connection task's answer.
///
/// Comfortably above the connection's own one-second wire budget and far
/// below the watch window, so a wedged task costs the probe its verdict
/// rather than the steering loop its minute.
const ACK_WAIT_SECS: u64 = 5;

/// Records the verdict once, in one place, so no path can return without one.
fn finish(
    arm: ProbeArm,
    connection_index: u8,
    instrument: SubscribeInstrument,
    verdict: ProbeVerdict,
) -> ProbeVerdict {
    metrics::counter!(
        PROBE_RUN_METRIC,
        "arm" => arm.as_str(),
        "verdict" => verdict.as_str(),
    )
    .increment(1);
    // A conclusive verdict is a vendor-facing finding and gets an `error!` so
    // it lands in the coded-error stream a support ticket is written from; an
    // inconclusive one is the probe declining to answer and is `info!`,
    // because paging an operator to say "we learned nothing" is the noise
    // this repository keeps retiring.
    if verdict.is_conclusive() {
        error!(
            code = ErrorCode::WsGapSubscriptionBatching.code_str(),
            source = "probe_verdict",
            arm = arm.as_str(),
            verdict = verdict.as_str(),
            connection_index,
            security_id = instrument.security_id,
            segment = instrument.segment.as_str(),
            baseline_secs = PROBE_BASELINE_SECS,
            watch_secs = PROBE_WATCH_SECS,
            "unsubscribe probe verdict, and it is quotable as it stands. The contract was \
             demonstrably arriving during the baseline, so the watch result is admissible; \
             and the connection's dial counter was compared across the window, so a \
             re-dial cannot be hiding inside this answer — it would have produced an \
             inconclusive verdict instead. No manual `ws_event_audit` check is needed."
        );
    } else {
        info!(
            source = "probe_verdict",
            arm = arm.as_str(),
            verdict = verdict.as_str(),
            connection_index,
            security_id = instrument.security_id,
            "unsubscribe probe declined to answer — no verdict may be quoted from this run"
        );
    }
    verdict
}

/// Runs every arm the config asks for, once, and leaves the sockets restored.
///
/// The two arms take DIFFERENT sockets when more than one is available: run
/// on the same socket they would confound each other, because Arm A leaves
/// the guard empty and Arm B's whole mechanism is the replay of that guard.
/// With only one steerable socket the second arm is skipped and says so.
pub async fn run_configured(cfg: &DepthUnsubscribeProbeConfig, sockets: &mut [RebalanceSocket]) {
    if !cfg.enabled {
        return;
    }
    let mut used: Vec<usize> = Vec::new();
    for arm in [ProbeArm::Unsubscribe, ProbeArm::SocketClose] {
        if !arm_enabled(cfg, arm) {
            continue;
        }
        let Some(index) = pick_target(sockets, &used) else {
            info!(
                source = "probe_no_target",
                arm = arm.as_str(),
                "unsubscribe probe skipped this arm: no depth-200 socket left holding a \
                 contract. Running two arms on ONE socket would confound them — Arm A \
                 empties the guard and Arm B measures the replay of that guard."
            );
            continue;
        };
        used.push(index);
        let Some(socket) = sockets.get_mut(index) else {
            continue;
        };
        let _ = run_arm(socket, arm).await;
    }
}

/// Whether the config asks for this arm.
#[must_use]
pub const fn arm_enabled(cfg: &DepthUnsubscribeProbeConfig, arm: ProbeArm) -> bool {
    match arm {
        ProbeArm::Unsubscribe => cfg.unsubscribe_arm,
        ProbeArm::SocketClose => cfg.socket_close_arm,
    }
}

// ---------------------------------------------------------------------------
// The once-per-DAY latch (2026-09-13)
// ---------------------------------------------------------------------------
//
// The steering loop's `probe_run` flag is once-per-PROCESS, and on this box
// those are NOT the same thing. The unit restarts on every deploy and on an
// OOM kill (`daily-universe-scope-expansion-2026-05-27.md` records ten
// consecutive OOM restarts on 2026-09-02), so a session that restarts at
// 11:00 re-arms a probe that already spent its one shot at 09:20 — emptying a
// SECOND depth-200 socket on a day the operator authorized one measurement.
// The scope lock's own REJECT list forbids running "more than once per
// session", and an in-memory flag cannot enforce that across a restart.
//
// So the latch is a marker FILE holding the IST day it was spent on. It is
// read ONCE at loop entry (the in-memory flag covers every iteration after
// that, so this is one small read per process, never per iteration) and
// written ONCE, immediately BEFORE the arm runs.
//
// **Written before, never after, and that ordering is the safety property.**
// A crash mid-probe leaves the day marked spent. That is the fail-closed
// direction: declining a re-run costs one measurement on a day the operator
// can re-arm tomorrow, while permitting one costs a second emptied socket on
// a live steering path. The asymmetry is the whole argument.
//
// A write that FAILS degrades to exactly today's behaviour — once per process
// — and says so once. It never degrades to more.

/// Directory the day latch lives in — the same cache directory the depth seed
/// uses, so a wipe that clears one clears the other.
const DAY_LATCH_DIR: &str = "data/instrument-cache";

/// The marker file. Its whole contents are one IST `YYYYMMDD`.
const DAY_LATCH_FILE: &str = "depth-unsubscribe-probe-day.txt";

/// Full path to the day latch.
#[must_use]
pub fn day_latch_path() -> std::path::PathBuf {
    std::path::Path::new(DAY_LATCH_DIR).join(DAY_LATCH_FILE)
}

/// Today's IST date as a `YYYYMMDD` integer, resolved at the moment of the
/// call rather than at boot.
///
/// Deliberately NOT the steering loop's boot-time `today_ymd`: a process that
/// spans midnight would compare against the day it started, and mark the wrong
/// day spent. The box stops at 17:30 IST so that should not happen — "should
/// not" is why this is computed fresh rather than inherited.
#[must_use]
pub fn today_ymd_ist() -> u32 {
    crate::dhan_feed_stack::ymd_from_ist_date(&crate::dhan_universe::today_ist_date())
}

/// Parse a day-latch file's contents.
///
/// Returns `None` for anything that is not a plain `YYYYMMDD` — an empty
/// file, a truncated write, a line of prose someone left behind. `None` means
/// "unknown", and an unknown latch is treated as NOT spent: refusing to run
/// on the strength of a corrupt file would make an operator-armed measurement
/// silently impossible with no way to tell why.
#[must_use]
pub fn parse_day_latch(contents: &str) -> Option<u32> {
    let trimmed = contents.trim();
    if trimmed.len() != 8 || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    trimmed.parse::<u32>().ok()
}

/// Has the probe already spent its one shot on `today`?
///
/// O(1): one read of an 8-byte file, called once per process. An absent or
/// unreadable file reads as NOT spent — the same direction `parse_day_latch`
/// takes, and for the same reason.
///
/// Takes the latch PATH rather than deriving it, so a test can exercise the
/// real read against a temporary file. Deriving it internally would force
/// every test of this function to write the live cache directory — which
/// would spend the day's probe for real, on a developer's machine, as a side
/// effect of running the suite.
#[must_use]
pub fn probe_already_ran_today(latch: &std::path::Path, today: u32) -> bool {
    match std::fs::read_to_string(latch) {
        Ok(body) => parse_day_latch(&body) == Some(today),
        Err(_) => false,
    }
}

/// Mark the day spent. Called BEFORE the arm runs — see the ordering argument
/// at the top of this section.
///
/// Best-effort by design. A failed write leaves the process with only its
/// in-memory flag, which is today's behaviour, and emits one coded line so the
/// degrade is visible rather than assumed.
///
/// Takes the latch PATH for the same reason `probe_already_ran_today` does.
pub fn mark_probe_ran_today(latch: &std::path::Path, today: u32) {
    if let Some(dir) = latch.parent()
        && let Err(err) = std::fs::create_dir_all(dir)
    {
        tracing::warn!(
            code = ErrorCode::WsGapSubscriptionBatching.code_str(),
            source = "probe_day_latch_unwritable",
            error = %err,
            dir = %dir.display(),
            "could not create the probe's day-latch directory — the probe is now once per \
             PROCESS rather than once per DAY, so a restart today could arm it a second time"
        );
        return;
    }
    if let Err(err) = std::fs::write(latch, format!("{today}")) {
        tracing::warn!(
            code = ErrorCode::WsGapSubscriptionBatching.code_str(),
            source = "probe_day_latch_unwritable",
            error = %err,
            path = %latch.display(),
            "could not write the probe's day-latch — the probe is now once per PROCESS \
             rather than once per DAY, so a restart today could arm it a second time"
        );
        return;
    }
    info!(
        source = "probe_day_latch_written",
        day = today,
        path = %latch.display(),
        "unsubscribe probe day latch written BEFORE the arm ran — a restart today will not \
         re-arm it"
    );
}
#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::types::ExchangeSegment;

    fn si(id: u64) -> SubscribeInstrument {
        SubscribeInstrument {
            security_id: id,
            segment: ExchangeSegment::NseFno,
        }
    }

    fn socket(connection_index: u8, held: Option<SubscribeInstrument>) -> RebalanceSocket {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        std::mem::forget(rx);
        RebalanceSocket {
            tx,
            connection_index,
            held,
            pending: None,
        }
    }

    /// The window ordering is the one thing a flag could not keep true, so it
    /// is a const-assert AND a test: a watch that outlives the ghost grace
    /// hands Arm A's measurement to Arm B's mechanism.
    #[test]
    fn the_watch_window_ends_before_the_ghost_detector_would_redial() {
        assert!(
            (PROBE_WATCH_SECS as i64) < crate::depth_subscription_view::GHOST_GRACE_SECS,
            "the ghost detector re-dials a socket still delivering a dropped contract past \
             the grace — inside the watch that is Arm B running under Arm A's label"
        );
    }

    /// The two arms must land on DIFFERENT sockets. Arm A leaves the guard
    /// empty and Arm B measures what the replay of that guard does, so one
    /// socket for both measures neither.
    #[test]
    fn pick_target_skips_a_socket_the_other_arm_already_used() {
        let sockets = vec![
            socket(10, Some(si(1))),
            socket(11, Some(si(2))),
            socket(12, None),
        ];
        let first = pick_target(&sockets, &[]).expect("a holding socket");
        let second = pick_target(&sockets, &[first]).expect("a second holding socket");
        assert_ne!(first, second);
        assert_eq!(first, 0);
        assert_eq!(second, 1);
    }

    /// A socket holding nothing is never a target: dropping nothing measures
    /// nothing, and the baseline would report a thin book for a contract that
    /// does not exist.
    #[test]
    fn pick_target_never_picks_a_socket_holding_nothing() {
        let sockets = vec![socket(10, None), socket(11, None)];
        assert_eq!(pick_target(&sockets, &[]), None);
    }

    /// With ONE steerable socket the second arm has nowhere to go and is
    /// skipped rather than doubled up.
    #[test]
    fn pick_target_returns_nothing_once_every_socket_is_used() {
        let sockets = vec![socket(10, Some(si(1)))];
        let first = pick_target(&sockets, &[]).expect("the only socket");
        assert_eq!(pick_target(&sockets, &[first]), None);
    }

    /// Only two verdicts say anything about the vendor. A ticket written from
    /// any other one is written from nothing.
    #[test]
    fn only_honoured_and_ignored_are_conclusive() {
        assert!(ProbeVerdict::Honoured.is_conclusive());
        assert!(ProbeVerdict::Ignored.is_conclusive());
        for verdict in [
            ProbeVerdict::InconclusiveThinBook,
            ProbeVerdict::InconclusiveRefused,
            ProbeVerdict::InconclusiveWireFailed,
            ProbeVerdict::InconclusiveNoTarget,
        ] {
            assert!(
                !verdict.is_conclusive(),
                "{} is the probe declining to answer",
                verdict.as_str()
            );
        }
    }

    /// Every verdict and arm carries a distinct, stable label — they are
    /// metric label values and a collision would silently merge two outcomes.
    #[test]
    fn every_verdict_label_is_distinct() {
        let labels = [
            ProbeVerdict::Honoured.as_str(),
            ProbeVerdict::Ignored.as_str(),
            ProbeVerdict::InconclusiveThinBook.as_str(),
            ProbeVerdict::InconclusiveRefused.as_str(),
            ProbeVerdict::InconclusiveWireFailed.as_str(),
            ProbeVerdict::InconclusiveNoTarget.as_str(),
        ];
        for (i, a) in labels.iter().enumerate() {
            for b in labels.iter().skip(i + 1) {
                assert_ne!(a, b, "two verdicts share a metric label value");
            }
        }
        assert_ne!(
            ProbeArm::Unsubscribe.as_str(),
            ProbeArm::SocketClose.as_str()
        );
    }

    /// A disabled config must not touch a single socket — the probe empties a
    /// depth-200 socket for a minute, and that is never something a default
    /// build should do.
    #[tokio::test(start_paused = true)]
    async fn run_configured_does_nothing_at_all_when_the_probe_is_disabled() {
        let cfg = DepthUnsubscribeProbeConfig::default();
        assert!(!cfg.enabled, "the default must be OFF");
        let mut sockets = vec![socket(10, Some(si(1)))];
        run_configured(&cfg, &mut sockets).await;
        assert_eq!(
            sockets[0].held,
            Some(si(1)),
            "a disabled probe must leave the socket exactly as it found it"
        );
    }

    /// A socket whose connection task is gone cannot be measured, and the
    /// probe must say so rather than reporting silence as a vendor answer.
    #[tokio::test(start_paused = true)]
    async fn run_arm_is_inconclusive_never_honoured_when_the_channel_is_dead() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        let mut s = RebalanceSocket {
            tx,
            connection_index: 10,
            held: Some(si(1)),
            pending: None,
        };
        // Straight to the action: the baseline is exercised by the live path,
        // and what this pins is that a dead channel can never yield a verdict
        // a ticket could be written from.
        let outcome = act_drop(&mut s, si(1), true).await;
        assert_eq!(outcome, Err(ProbeVerdict::InconclusiveWireFailed));
        assert_eq!(
            s.held,
            Some(si(1)),
            "nothing reached the wire, so believed-held must not move"
        );
    }

    // -----------------------------------------------------------------
    // The two verdict-corrupting defects (2026-09-13)
    // -----------------------------------------------------------------
    //
    // Both produced the SAME wrong answer — `Honoured` — from two different
    // causes, and `Honoured` is the answer that would close a vendor ticket
    // in the vendor's favour on evidence we do not have.

    /// Silence is only evidence if the socket was not re-dialled underneath
    /// the measurement.
    ///
    /// The ghost detector, a supervisor reconnect, or Arm B's own close all
    /// re-dial a socket and replay a set that EXCLUDES the dropped contract.
    /// The stream then stops for a reason that has nothing to do with the
    /// unsubscribe, and silence reads as "the vendor honoured it". The dial
    /// generation counter is what separates the two.
    #[test]
    fn arm_a_silence_after_a_redial_is_inconclusive_not_honoured() {
        assert_eq!(
            verdict_for(ProbeArm::Unsubscribe, FrameWatch::Silent, 7, 7),
            ProbeVerdict::Honoured,
            "no redial happened, so the silence is the unsubscribe's"
        );
        assert_eq!(
            verdict_for(ProbeArm::Unsubscribe, FrameWatch::Silent, 7, 8),
            ProbeVerdict::InconclusiveRedialled,
            "the socket re-dialled during the watch — the replay excluded the contract, so \
             the silence is OUR mechanism and says nothing about the vendor's"
        );
    }

    /// Arm B's silence is only evidence if the close-and-redial actually
    /// happened.
    ///
    /// The mirror of the case above, and it fails the other way: if the
    /// socket never re-dialled, Arm B did not run its mechanism at all, and
    /// calling that `Honoured` credits a close that never occurred.
    #[test]
    fn arm_b_silence_without_a_redial_is_inconclusive_not_honoured() {
        assert_eq!(
            verdict_for(ProbeArm::SocketClose, FrameWatch::Silent, 3, 4),
            ProbeVerdict::Honoured,
            "the socket re-dialled, so Arm B's mechanism ran and the silence is its result"
        );
        assert_eq!(
            verdict_for(ProbeArm::SocketClose, FrameWatch::Silent, 3, 3),
            ProbeVerdict::InconclusiveNotRedialled,
            "no redial — Arm B never ran, and silence from a mechanism that did not fire \
             is not evidence that the mechanism works"
        );
    }

    /// A REFUSED first-packet stamp is not a frame.
    ///
    /// `record_subscribe_at` refuses when the contract may already be
    /// streaming, or when the pending set is at its cap. Before this fix the
    /// refusal was indistinguishable from an accepted stamp that never
    /// resolved, so an unmeasured window reported as `Honoured` — a verdict
    /// manufactured out of a bookkeeping refusal.
    #[test]
    fn an_unmeasured_window_is_never_a_verdict() {
        for arm in [ProbeArm::Unsubscribe, ProbeArm::SocketClose] {
            for (before, after) in [(1_u64, 1_u64), (1, 2)] {
                assert_eq!(
                    verdict_for(arm, FrameWatch::NotMeasured, before, after),
                    ProbeVerdict::InconclusiveNotMeasured,
                    "an unmeasured watch must never resolve to Honoured or Ignored, on \
                     either arm, redialled or not"
                );
            }
        }
    }

    /// An arriving frame is decisive on both arms and needs no redial check.
    ///
    /// This is the one direction a redial cannot fake: a replay that excluded
    /// the contract cannot produce the contract's own packets.
    #[test]
    fn an_arriving_frame_is_ignored_on_either_arm() {
        for arm in [ProbeArm::Unsubscribe, ProbeArm::SocketClose] {
            for (before, after) in [(0_u64, 0_u64), (0, 5)] {
                assert_eq!(
                    verdict_for(arm, FrameWatch::Arrived, before, after),
                    ProbeVerdict::Ignored,
                    "the contract kept streaming — that is the finding, whatever the \
                     socket did meanwhile"
                );
            }
        }
    }

    /// A dial generation only ever moves forward, and a wrapped or
    /// backwards reading must not be read as "no redial".
    #[test]
    fn a_generation_that_went_backwards_reads_as_no_redial_not_as_a_redial() {
        assert_eq!(
            verdict_for(ProbeArm::Unsubscribe, FrameWatch::Silent, 9, 4),
            ProbeVerdict::Honoured,
            "strictly-greater is the test; a backwards reading is not a redial"
        );
    }

    /// Arm B must remove the contract from the retained set WITHOUT sending
    /// an unsubscribe frame — that is the whole difference between the arms.
    ///
    /// # The defect this pins (2026-09-13)
    ///
    /// Arm B is the CONTROL. It answers "can we stop a stream at all?" by
    /// closing the socket and replaying a set that excludes the contract. If
    /// it also sends `RequestCode` 25 on the way out, then a silent socket
    /// afterwards could be the close OR the unsubscribe, and the arm that
    /// exists to isolate our mechanism from the vendor's has confounded
    /// exactly those two. Before the `send_wire` flag it sent the frame.
    ///
    /// The assertion is on the WIRE, not on the belief: both arms must still
    /// stop believing they hold the contract, because the retained set is
    /// what the redial replays.
    #[tokio::test(start_paused = true)]
    async fn arm_b_drops_the_contract_without_sending_an_unsubscribe_frame() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut s = RebalanceSocket {
            tx,
            connection_index: 11,
            held: Some(si(42)),
            pending: None,
        };

        // Answer the ack the way a live connection task would. Without this
        // the drop times out and the test measures the timeout instead of the
        // arm — which is exactly what the FIRST version of this test did.
        let responder = tokio::spawn(async move {
            let cmd = rx
                .recv()
                .await
                .expect("the drop command must reach the supervisor");
            match cmd {
                LiveSubscriptionCommand::ProbeUnsubscribe { send_wire, ack, .. } => {
                    if let Some(ack) = ack {
                        let _ = ack.send(ProbeUnsubscribeOutcome::Dropped);
                    }
                    send_wire
                }
                other => panic!("expected a ProbeUnsubscribe command, got {other:?}"),
            }
        });

        let outcome = act_drop(&mut s, si(42), false).await;
        let send_wire = responder.await.expect("the responder task panicked");

        assert!(
            outcome.is_ok(),
            "a wire-free drop the supervisor confirmed must not report a failure"
        );
        assert_eq!(
            s.held, None,
            "the retained set must still lose the contract — the redial replays that set, \
             and a contract still in it comes straight back"
        );
        assert!(
            !send_wire,
            "Arm B asked the supervisor to SEND the unsubscribe — the control arm just \
             became a second copy of Arm A, and a silent socket afterwards no longer \
             tells the two mechanisms apart"
        );
    }

    /// Arm A is the mirror: it MUST put the frame on the wire, or it measures
    /// nothing about the vendor at all.
    #[tokio::test(start_paused = true)]
    async fn arm_a_sends_the_unsubscribe_frame() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut s = RebalanceSocket {
            tx,
            connection_index: 12,
            held: Some(si(43)),
            pending: None,
        };
        let _ = act_drop(&mut s, si(43), true).await;
        let cmd = rx
            .try_recv()
            .expect("the drop command must reach the supervisor");
        match cmd {
            LiveSubscriptionCommand::ProbeUnsubscribe { send_wire, .. } => assert!(
                send_wire,
                "Arm A stopped sending the unsubscribe — it is now measuring our own \
                 bookkeeping and reporting the answer as the vendor's"
            ),
            other => panic!("expected a ProbeUnsubscribe command, got {other:?}"),
        }
    }

    /// A scratch latch path unique to this test binary, so the suite never
    /// writes the LIVE cache directory. A test that wrote the real latch
    /// would spend the day's one probe for real, as a side effect of running
    /// `cargo test` — which is exactly why these two functions take a path.
    fn scratch_latch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!("tv-probe-latch-{}-{name}", std::process::id()))
            .join(DAY_LATCH_FILE)
    }

    #[test]
    fn day_latch_path_sits_in_the_cache_the_depth_seed_also_uses() {
        let path = day_latch_path();
        // The directory is shared with the depth seed on purpose: a wipe that
        // clears one must clear the other, or a restored seed would pair with
        // a stale latch.
        assert_eq!(
            path.parent().and_then(|p| p.to_str()),
            Some(DAY_LATCH_DIR),
            "the day latch moved out of the depth-seed cache directory"
        );
        assert_eq!(
            path.file_name().and_then(|f| f.to_str()),
            Some(DAY_LATCH_FILE)
        );
    }

    #[test]
    fn today_ymd_ist_returns_a_plausible_eight_digit_ist_date() {
        let today = today_ymd_ist();
        // Bounded rather than pinned: pinning a date would rot tomorrow.
        assert!(
            (20_000_101..=29_991_231).contains(&today),
            "today_ymd_ist returned {today}, which is not a YYYYMMDD in this century"
        );
        let month = (today / 100) % 100;
        let day = today % 100;
        assert!((1..=12).contains(&month), "month {month} out of range");
        assert!((1..=31).contains(&day), "day {day} out of range");
        // It must agree with what it claims to wrap, or a midnight-spanning
        // process would mark the wrong day spent.
        assert_eq!(
            today,
            crate::dhan_feed_stack::ymd_from_ist_date(&crate::dhan_universe::today_ist_date())
        );
    }

    #[test]
    fn parse_day_latch_accepts_only_a_bare_eight_digit_date() {
        assert_eq!(parse_day_latch("20260913"), Some(20_260_913));
        // Trailing newline is what `std::fs::write` + an editor leave behind.
        assert_eq!(parse_day_latch("20260913\n"), Some(20_260_913));
        assert_eq!(parse_day_latch("  20260913  "), Some(20_260_913));

        // Every one of these must read as UNKNOWN, never as a date: an empty
        // file, a torn write, a leftover line of prose, a wrong width.
        for junk in [
            "",
            "\n",
            "2026091",
            "202609133",
            "2026-09-13",
            "todays date is 20260913",
            "abcdefgh",
            "2026 913",
            "+2026091",
        ] {
            assert_eq!(
                parse_day_latch(junk),
                None,
                "parse_day_latch accepted {junk:?} as a date"
            );
        }
    }

    #[test]
    fn probe_already_ran_today_reads_an_absent_or_corrupt_latch_as_not_spent() {
        let latch = scratch_latch("absent");
        let _ = std::fs::remove_file(&latch);

        // Absent: the common case on the first boot of the day, and after a
        // cache wipe. Must NOT read as spent — refusing to run on the
        // strength of a missing file makes an armed measurement impossible.
        assert!(!probe_already_ran_today(&latch, 20_260_913));

        // Corrupt: same direction, same reason.
        if let Some(dir) = latch.parent() {
            std::fs::create_dir_all(dir).expect("scratch dir");
        }
        std::fs::write(&latch, "not a date at all").expect("write junk");
        assert!(!probe_already_ran_today(&latch, 20_260_913));

        // A latch from a PREVIOUS day is also not spent — that is the whole
        // point of it being a day latch rather than a boolean.
        std::fs::write(&latch, "20260912").expect("write yesterday");
        assert!(!probe_already_ran_today(&latch, 20_260_913));

        let _ = std::fs::remove_file(&latch);
    }

    #[test]
    fn mark_probe_ran_today_then_probe_already_ran_today_round_trips() {
        let latch = scratch_latch("roundtrip");
        let _ = std::fs::remove_file(&latch);
        assert!(!probe_already_ran_today(&latch, 20_260_913));

        // The mark must create its own directory — on a wiped box the cache
        // directory does not exist yet.
        mark_probe_ran_today(&latch, 20_260_913);
        assert!(
            probe_already_ran_today(&latch, 20_260_913),
            "the day latch did not survive its own write"
        );

        // It spends THAT day only: tomorrow's boot re-arms.
        assert!(!probe_already_ran_today(&latch, 20_260_914));

        // Idempotent — a second mark on the same day is not an error.
        mark_probe_ran_today(&latch, 20_260_913);
        assert!(probe_already_ran_today(&latch, 20_260_913));

        let _ = std::fs::remove_file(&latch);
    }

    #[test]
    fn mark_probe_ran_today_degrades_quietly_when_the_path_is_unwritable() {
        // A latch whose PARENT is an existing FILE cannot be created. The
        // contract is that this degrades to once-per-process and says so,
        // never that it panics on a live steering path (`panic = "abort"`).
        let blocker =
            std::env::temp_dir().join(format!("tv-probe-latch-blocker-{}", std::process::id()));
        std::fs::write(&blocker, b"i am a file, not a directory").expect("write blocker");
        let latch = blocker.join(DAY_LATCH_FILE);

        mark_probe_ran_today(&latch, 20_260_913);
        assert!(
            !probe_already_ran_today(&latch, 20_260_913),
            "a latch that could not be written must not read as spent"
        );

        let _ = std::fs::remove_file(&blocker);
    }
}

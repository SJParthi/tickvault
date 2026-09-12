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
    LiveSubscriptionCommand, ProbeUnsubscribeOutcome, SubscribeInstrument, request_probe_close,
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
) -> bool {
    let tracker = crate::depth_first_packet::global_depth_first_packet_tracker();
    tracker.record_subscribe_at(
        instrument.security_id,
        instrument.segment,
        DepthFeedKind::TwoHundred,
        started_nanos,
        false,
    );
    tokio::time::sleep(window).await;
    // `forget` returns true when the entry was STILL THERE — i.e. no packet
    // resolved it. The window is shorter than the tracker's own sweep, so a
    // `false` here can only mean a packet arrived, never that it aged out.
    !tracker.forget(
        instrument.security_id,
        instrument.segment,
        DepthFeedKind::TwoHundred,
    )
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
    if !any_frame_within(
        instrument,
        Duration::from_secs(PROBE_BASELINE_SECS),
        now_nanos,
    )
    .await
    {
        return finish(
            arm,
            connection_index,
            instrument,
            ProbeVerdict::InconclusiveThinBook,
        );
    }

    let acted = match arm {
        ProbeArm::Unsubscribe => act_unsubscribe(socket, instrument).await,
        ProbeArm::SocketClose => act_socket_close(connection_index),
    };
    if let Err(verdict) = acted {
        return finish(arm, connection_index, instrument, verdict);
    }

    let watch_started = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let arrived = any_frame_within(
        instrument,
        Duration::from_secs(PROBE_WATCH_SECS),
        watch_started,
    )
    .await;

    // Arm A emptied the socket, so it must be put back. Arm B did not touch
    // the guard — the re-dial replays it — so there is nothing to restore.
    if arm == ProbeArm::Unsubscribe {
        restore(socket, instrument).await;
    }

    let verdict = if arrived {
        ProbeVerdict::Ignored
    } else {
        ProbeVerdict::Honoured
    };
    finish(arm, connection_index, instrument, verdict)
}

/// Arm A: drop the contract and put nothing back.
async fn act_unsubscribe(
    socket: &mut RebalanceSocket,
    instrument: SubscribeInstrument,
) -> Result<(), ProbeVerdict> {
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    if socket
        .tx
        .try_send(LiveSubscriptionCommand::ProbeUnsubscribe {
            drop_this: instrument,
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

/// Arm B: close the socket and let the ladder bring it back.
///
/// The guard is UNTOUCHED, so the replay re-subscribes the same contract —
/// which is what makes this a different question from Arm A rather than a
/// louder version of it. Arm B asks whether a stream survives a full close
/// and re-dial; if frames never stop, the problem is not the request code.
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
            "unsubscribe probe verdict. The contract was demonstrably arriving during the \
             baseline, so the watch result is admissible. ⚠ ONE residual the reader must \
             check before quoting this: a RE-DIAL of this connection inside the watch \
             window would produce a false `honoured` for Arm A, because the emptied guard \
             replays nothing — confirm against `ws_event_audit` for this connection_index \
             over the window above."
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
        let outcome = act_unsubscribe(&mut s, si(1)).await;
        assert_eq!(outcome, Err(ProbeVerdict::InconclusiveWireFailed));
        assert_eq!(
            s.held,
            Some(si(1)),
            "nothing reached the wire, so believed-held must not move"
        );
    }
}

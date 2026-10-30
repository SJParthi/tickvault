//! Coverage tests for `pool_supervisor` (part 1): the log and counter halves
//! of decisions the module already makes, the 808 refresh-once park stages,
//! the subscribe-guard refusals, the overflow-probe reporting, and the
//! in-place resubscribe engine (`apply_resubscribe`) over a scripted socket.
//!
//! Every test asserts the outcome (the action, the guard, the ack, the
//! counter and the coded log line), never only that a line ran. Logs are
//! captured with a thread-local `tracing` subscriber and counters with a
//! scoped `metrics` recorder, so nothing process-wide is installed.
//!
//! Process-wide registers: only out-of-range indices are written (which the
//! registers refuse), except slot 26, which no real pool owns (the two
//! accounts tile slots 0..26) and no other test in this module writes for
//! the registers used here.

use super::*;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Capture helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct LogBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut g) = self.0.lock() {
            g.extend_from_slice(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

type CounterMap = Arc<Mutex<HashMap<String, u64>>>;

#[derive(Clone, Default)]
struct Counters(CounterMap);

struct CovCounter {
    key: String,
    counts: CounterMap,
}

impl metrics::CounterFn for CovCounter {
    fn increment(&self, value: u64) {
        if let Ok(mut m) = self.counts.lock() {
            *m.entry(self.key.clone()).or_insert(0) += value;
        }
    }
    fn absolute(&self, value: u64) {
        if let Ok(mut m) = self.counts.lock() {
            m.insert(self.key.clone(), value);
        }
    }
}

impl metrics::Recorder for Counters {
    fn describe_counter(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn describe_gauge(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn describe_histogram(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn register_counter(&self, key: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Counter {
        let mut rendered = key.name().to_string();
        for label in key.labels() {
            rendered.push(',');
            rendered.push_str(label.key());
            rendered.push('=');
            rendered.push_str(label.value());
        }
        metrics::Counter::from_arc(Arc::new(CovCounter {
            key: rendered,
            counts: Arc::clone(&self.0),
        }))
    }
    fn register_gauge(&self, _: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Gauge {
        metrics::Gauge::noop()
    }
    fn register_histogram(
        &self,
        _: &metrics::Key,
        _: &metrics::Metadata<'_>,
    ) -> metrics::Histogram {
        metrics::Histogram::noop()
    }
}

/// What one captured run produced.
struct Captured<R> {
    value: R,
    logs: String,
    counters: HashMap<String, u64>,
}

fn count(counters: &HashMap<String, u64>, key: &str) -> u64 {
    counters.get(key).copied().unwrap_or(0)
}

/// Runs `f` with a thread-local TRACE subscriber and a scoped recorder.
fn capture<R>(f: impl FnOnce() -> R) -> Captured<R> {
    let buf = LogBuf::default();
    let writer = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let recorder = Counters::default();
    let value = tracing::subscriber::with_default(subscriber, || {
        metrics::with_local_recorder(&recorder, f)
    });
    let logs = buf
        .0
        .lock()
        .map(|g| String::from_utf8_lossy(&g).into_owned())
        .unwrap_or_default();
    let counters = recorder.0.lock().map(|m| m.clone()).unwrap_or_default();
    Captured {
        value,
        logs,
        counters,
    }
}

/// [`capture`] around a paused current-thread runtime.
fn capture_async<R, F>(f: impl FnOnce() -> F) -> Captured<R>
where
    F: std::future::Future<Output = R>,
{
    capture(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .expect("runtime");
        runtime.block_on(f())
    })
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A slot no real pool owns (the two accounts tile 0..26).
const COV_SLOT: u8 = 26;

fn slot_at(endpoint: DhanEndpointType, global_index: u8) -> ConnectionSlot {
    ConnectionSlot {
        account: crate::websocket::pool_budget::DhanAccount::Primary,
        endpoint,
        pool_index: 0,
        global_index,
    }
}

fn inst(id: u64) -> SubscribeInstrument {
    SubscribeInstrument {
        security_id: id,
        segment: ExchangeSegment::NseFno,
    }
}

fn insts(ids: std::ops::Range<u64>) -> Vec<SubscribeInstrument> {
    ids.map(inst).collect()
}

fn live(endpoint: DhanEndpointType, global_index: u8, t: Instant) -> ConnectionSupervisor {
    let mut s = ConnectionSupervisor::new(slot_at(endpoint, global_index), t);
    let _ = s.on_event(ConnEvent::BeginDial, t);
    let _ = s.on_event(ConnEvent::DialSucceeded, t);
    let _ = s.on_event(ConnEvent::SubscribeAcked, t);
    assert_eq!(s.phase(), ConnPhase::Live);
    s
}

fn is_redial(a: &SupervisorAction) -> bool {
    matches!(a, SupervisorAction::SleepThenDial { .. })
}

fn spacing0() -> [std::sync::atomic::AtomicI64; CONFIRMED_SPACING_KINDS] {
    [const { std::sync::atomic::AtomicI64::new(0) }; CONFIRMED_SPACING_KINDS]
}

// ---------------------------------------------------------------------------
// Pure helpers and small enums
// ---------------------------------------------------------------------------

#[test]
fn silence_inputs_live_reads_the_process_register() {
    let before = wall_clock_ms();
    let inputs = SilenceInputs::live();
    let after = wall_clock_ms();
    assert!(inputs.wall_ms >= before && inputs.wall_ms <= after);
    assert!(std::ptr::eq(inputs.spacing, &LAST_CONFIRMED_REDIAL_WALL_MS));
    assert_eq!(inputs.stamps.len(), GHOST_REDIAL_SLOTS);
}

#[test]
fn confirmed_min_held_is_unreachable_for_kinds_without_a_confirmed_path() {
    assert_eq!(confirmed_min_held(DhanEndpointType::Depth200), u32::MAX);
    assert_eq!(confirmed_min_held(DhanEndpointType::OrderUpdate), u32::MAX);
    assert_eq!(
        confirmed_min_held(DhanEndpointType::MainFeed),
        MAIN_FEED_CONFIRMED_MIN_HELD
    );
}

#[test]
fn live_siblings_stops_at_slot_256_of_an_oversized_stamp_slice() {
    let wall = 1_800_000_000_000_i64;
    // Every slot past index 255 is fresh but cannot be named by a u8 slot:
    // the walk stops there instead of wrapping onto slot 0.
    let mut stamps = vec![0_i64; 300];
    for s in stamps.iter_mut().skip(256) {
        *s = wall;
    }
    assert_eq!(
        live_siblings(0, DhanEndpointType::MainFeed, wall, &stamps),
        0
    );
    // Two real main-feed siblings still count.
    if let Some(s) = stamps.get_mut(1) {
        *s = wall;
    }
    if let Some(s) = stamps.get_mut(2) {
        *s = wall;
    }
    assert_eq!(
        live_siblings(0, DhanEndpointType::MainFeed, wall, &stamps),
        2
    );
}

#[test]
fn depth200_held_refuses_a_contract_published_with_no_start_time() {
    let a = inst(72_271);
    publish_depth200_held(COV_SLOT, Some(a), 0);
    assert_eq!(
        depth200_held(COV_SLOT),
        None,
        "a zero start time is never paired with an id"
    );
    publish_depth200_held(COV_SLOT, Some(inst(72_272)), 5_000);
    assert_eq!(
        depth200_held(COV_SLOT),
        Some((72_272, ExchangeSegment::NseFno, 5_000))
    );
    publish_depth200_held(COV_SLOT, None, 0);
    assert_eq!(depth200_held(COV_SLOT), None);
}

#[test]
fn data_silence_refusal_labels_are_stable() {
    assert_eq!(DataSilenceRefusal::OutOfRange.as_str(), "out_of_range");
    assert_eq!(DataSilenceRefusal::CoolingDown.as_str(), "cooling_down");
    assert_eq!(DataSilenceRefusal::StillPending.as_str(), "still_pending");
    assert_eq!(DataSilenceRefusal::Halted.as_str(), "halted");
}

#[test]
fn data_silence_register_round_trips_on_an_unowned_slot() {
    // Out of range: refused, nothing written.
    assert_eq!(
        request_data_silence_redial(200, 1_000),
        Err(DataSilenceRefusal::OutOfRange)
    );
    assert!(!take_data_silence_redial(200));
    // The live register on a slot no pool owns. Only refused when an 805
    // already halted rotation in this process (another test may have).
    let halted = rotation_halted();
    let outcome = request_data_silence_redial(COV_SLOT, 1_000);
    if halted {
        assert_eq!(outcome, Err(DataSilenceRefusal::Halted));
        assert!(!take_data_silence_redial(COV_SLOT));
        return;
    }
    let taken = capture(|| take_data_silence_redial(COV_SLOT));
    match outcome {
        Ok(()) => {
            assert!(taken.value, "a request made against this dial is taken");
            assert_eq!(count(&taken.counters, DATA_SILENCE_STALE_METRIC), 0);
        }
        // A concurrent 805 in another test can flip the latch between the
        // two reads; then nothing was recorded.
        Err(refusal) => {
            assert_eq!(refusal, DataSilenceRefusal::Halted);
            assert!(!taken.value);
        }
    }
    assert!(
        !take_data_silence_redial(COV_SLOT),
        "a take clears the slot"
    );
}

#[test]
fn data_silence_take_drops_and_counts_a_request_made_against_an_earlier_dial() {
    let pending = [const { std::sync::atomic::AtomicU64::new(0) }; GHOST_REDIAL_SLOTS];
    let last = [const { std::sync::atomic::AtomicI64::new(0) }; GHOST_REDIAL_SLOTS];
    assert_eq!(
        request_data_silence_redial_in(&pending, &last, 3, 10_000, false, 4),
        Ok(())
    );
    assert_eq!(
        take_data_silence_redial_in(&pending, 3, 5),
        DataSilenceTake::Stale
    );
    assert_eq!(
        take_data_silence_redial_in(&pending, 3, 5),
        DataSilenceTake::Nothing
    );
}

#[test]
fn continuous_session_gate_is_open_exactly_when_the_session_clock_says_so() {
    let gate = FrameSilenceGate::ContinuousSessionIst;
    let before = gate.is_open_at(super::super::silence_calendar::session_secs_of_day_now());
    let open = gate.is_open();
    let after = gate.is_open_at(super::super::silence_calendar::session_secs_of_day_now());
    assert!(open == before || open == after);
    assert!(FrameSilenceGate::AlwaysOn.is_open());
    assert!(!FrameSilenceGate::Off.is_open());
}

#[test]
fn a_fresh_ghost_register_holds_nothing_and_round_trips_one_request() {
    let reg = GhostRegister::new();
    for idx in [0u8, 15, 31, 200] {
        assert_eq!(reg.take(idx), None);
    }
    let dflt = GhostRegister::default();
    assert_eq!(dflt.take(5), None);
    let code = ExchangeSegment::NseFno.binary_code();
    assert_eq!(dflt.request(5, 72_271, code, 1_000_000), Ok(()));
    assert_eq!(dflt.take(5), Some(inst(72_271)));
    assert_eq!(dflt.take(5), None, "taken once");
}

#[test]
fn refused_voluntary_redial_after_805_is_counted_and_logged_coded() {
    let c = capture(|| {
        refuse_voluntary_redial_after_805(slot_at(DhanEndpointType::Depth200, 12), "rotate");
    });
    assert_eq!(
        count(
            &c.counters,
            &format!("{DIAL_REFUSED_AFTER_805_METRIC},path=rotate")
        ),
        1
    );
    assert!(c.logs.contains("ERROR"));
    assert!(c.logs.contains("dial_refused_after_805"));
    assert!(
        c.logs
            .contains(ErrorCode::WsGapDisconnectClassification.code_str())
    );
    assert!(c.logs.contains("depth_200"), "{}", c.logs);
}

#[test]
fn probe_close_refusal_labels_and_overflow_register_reads() {
    assert_eq!(ProbeCloseRefusal::OutOfRange.as_str(), "out_of_range");
    assert_eq!(ProbeCloseRefusal::AlreadyArmed.as_str(), "already_armed");
    // The order-update socket never takes part: always "down".
    assert!(overflow_episode_down_for(DhanEndpointType::OrderUpdate));
    assert_eq!(
        overflow_episode_down_for(DhanEndpointType::MainFeed),
        OVERFLOW_DOWN.load(std::sync::atomic::Ordering::Acquire)
    );
    for endpoint in [DhanEndpointType::Depth20, DhanEndpointType::Depth200] {
        assert_eq!(
            overflow_episode_down_for(endpoint),
            DEPTH_OVERFLOW_DOWN.load(std::sync::atomic::Ordering::Acquire)
        );
    }
    // Out-of-range slots hold no grant and no re-park request.
    assert!(!take_overflow_grant(200));
    assert!(!take_overflow_repark(200));
    assert!(overflow_probe_window_open(FrameSilenceGate::AlwaysOn));
    assert!(!overflow_probe_window_open(FrameSilenceGate::Off));
}

// ---------------------------------------------------------------------------
// Overflow episode core (local values, never the process-wide instance)
// ---------------------------------------------------------------------------

#[test]
fn a_waiting_episode_past_its_probe_schedule_fails_closed_to_down_for_session() {
    let now = Instant::now();
    for mut ep in [OverflowEpisode::new(), OverflowEpisode::new_depth()] {
        ep.phase = OverflowEpisodePhase::Waiting;
        ep.probes_started = u8::MAX;
        ep.phase_since = Some(now);
        let effect = ep.poll(now, true);
        assert_eq!(ep.phase, OverflowEpisodePhase::DownForSession);
        assert_eq!(
            effect.outcomes,
            [Some(OverflowProbeOutcome::DownForSession), None]
        );
        assert_eq!(effect.grant, None);
    }
}

#[test]
fn a_restarted_watch_past_the_window_bound_fails_as_deferral_exhausted() {
    let now = Instant::now();
    let mut ep = OverflowEpisode::new();
    ep.phase = OverflowEpisodePhase::Probing;
    ep.probes_started = 1;
    ep.watched_slot = 3;
    // The watch restarted just now (fresh deadline), but the grant itself is
    // past the absolute window bound.
    ep.phase_since = Some(now);
    ep.granted_at = Some(now - Duration::from_secs(OVERFLOW_PROBE_WINDOW_MAX_SECS + 1));
    ep.first_frame_at = None;
    let effect = ep.poll(now, true);
    assert_eq!(
        effect.outcomes.first().copied().flatten(),
        Some(OverflowProbeOutcome::FailedDeferralExhausted)
    );
    assert_eq!(
        effect.repark,
        Some(3),
        "the failed probe is asked to park again"
    );
    assert_eq!(effect.sibling, None, "no noted sibling to name");
    assert_eq!(ep.phase, OverflowEpisodePhase::Waiting);
}

#[test]
fn take_lowest_parked_on_an_empty_mask_names_no_slot() {
    let mut ep = OverflowEpisode::new();
    assert_eq!(ep.take_lowest_parked(), u8::MAX);
    ep.on_parked(4);
    assert_eq!(ep.take_lowest_parked(), 4);
    assert_eq!(ep.take_lowest_parked(), u8::MAX);
}

#[test]
fn an_order_update_park_joins_neither_episode() {
    let mut eps = OverflowEpisodes::new();
    eps.on_parked(DhanEndpointType::OrderUpdate, 15);
    assert_eq!(eps.main.parked_mask, 0);
    assert_eq!(eps.depth.parked_mask, 0);
    eps.on_parked(DhanEndpointType::Depth20, 6);
    assert_eq!(eps.depth.parked_mask, 1 << 6);
}

#[test]
fn every_probe_outcome_is_counted_and_logged_at_its_level() {
    let all = [
        (
            OverflowEpisodeKind::MainFeed,
            OverflowProbeOutcome::FailedNoFrame,
        ),
        (
            OverflowEpisodeKind::MainFeed,
            OverflowProbeOutcome::SiblingHealed,
        ),
        (
            OverflowEpisodeKind::Depth,
            OverflowProbeOutcome::ProbeGranted,
        ),
        (
            OverflowEpisodeKind::Depth,
            OverflowProbeOutcome::SiblingResetNoted,
        ),
    ];
    for (kind, outcome) in all {
        let c = capture(|| report_overflow_probe_outcome(kind, outcome, Some(3), Some(3), Some(4)));
        assert_eq!(
            count(
                &c.counters,
                &format!("{},outcome={}", kind.metric(), outcome.as_str())
            ),
            1,
            "{outcome:?}"
        );
        let level = if outcome.is_note() { "WARN" } else { "ERROR" };
        assert!(c.logs.contains(level), "{outcome:?}: {}", c.logs);
        assert!(c.logs.contains(kind.source()), "{}", c.logs);
        assert!(c.logs.contains(outcome.as_str()));
        assert!(c.logs.contains("sibling_connection=4"), "{}", c.logs);
        if outcome.is_note() {
            assert!(c.logs.contains(outcome.note_detail()));
        }
    }
    // Absent slots are logged as -1.
    let c = capture(|| {
        report_overflow_probe_outcome(
            OverflowEpisodeKind::Depth,
            OverflowProbeOutcome::DownForSession,
            None,
            None,
            None,
        );
    });
    assert!(c.logs.contains("connection_index=-1"), "{}", c.logs);
}

#[test]
fn apply_overflow_effect_reports_each_outcome_and_ignores_an_out_of_range_slot() {
    let effect = OverflowEpisodeEffect {
        outcomes: [
            Some(OverflowProbeOutcome::FailedNoFrame),
            Some(OverflowProbeOutcome::DownForSession),
        ],
        grant: None,
        repark: Some(200),
        sibling: None,
        restart_watch: false,
    };
    let c = capture(|| apply_overflow_effect(OverflowEpisodeKind::MainFeed, effect, Some(200)));
    for outcome in [
        OverflowProbeOutcome::FailedNoFrame,
        OverflowProbeOutcome::DownForSession,
    ] {
        assert_eq!(
            count(
                &c.counters,
                &format!("{OVERFLOW_PROBE_METRIC},outcome={}", outcome.as_str())
            ),
            1
        );
    }
    assert!(c.logs.contains("connection_index=200"), "{}", c.logs);
    let granted = OverflowEpisodeEffect {
        outcomes: [Some(OverflowProbeOutcome::ReleaseGranted), None],
        grant: Some(200),
        ..OverflowEpisodeEffect::default()
    };
    let c = capture(|| apply_overflow_effect(OverflowEpisodeKind::Depth, granted, None));
    assert_eq!(
        count(
            &c.counters,
            &format!(
                "{DEPTH_OVERFLOW_PROBE_METRIC},outcome={}",
                OverflowProbeOutcome::ReleaseGranted.as_str()
            )
        ),
        1
    );
    assert!(!take_overflow_grant(200));
}

// ---------------------------------------------------------------------------
// The supervisor's coded decisions
// ---------------------------------------------------------------------------

#[test]
fn a_socket_parked_for_805_is_released_by_the_probe_and_redials() {
    let t = Instant::now();
    for endpoint in [DhanEndpointType::MainFeed, DhanEndpointType::Depth200] {
        let mut s = live(endpoint, COV_SLOT, t);
        s.phase = ConnPhase::Parked;
        s.park_reason = Some(ParkReason::PoolOverflow);
        let c = capture(|| s.on_event(ConnEvent::OverflowProbeGranted, t));
        assert!(is_redial(&c.value), "{endpoint:?}: {:?}", c.value);
        assert_eq!(s.park_reason(), None);
        assert_eq!(s.last_redial_reason(), ReconnectReason::OverflowProbe);
        assert!(c.logs.contains("released by the overflow probe"));
    }
    // The order-update socket takes no part: still parked.
    let mut s = live(DhanEndpointType::OrderUpdate, 15, t);
    s.phase = ConnPhase::Parked;
    s.park_reason = Some(ParkReason::PoolOverflow);
    assert_eq!(
        s.on_event(ConnEvent::OverflowProbeGranted, t),
        SupervisorAction::Continue
    );
    assert_eq!(s.phase(), ConnPhase::Parked);
}

#[test]
fn an_overflow_probe_failure_never_reparks_the_order_update_socket() {
    let t = Instant::now();
    let mut s = live(DhanEndpointType::OrderUpdate, 15, t);
    assert_eq!(
        s.on_event(ConnEvent::OverflowProbeFailed, t),
        SupervisorAction::Continue
    );
    assert_eq!(s.phase(), ConnPhase::Live);
}

#[test]
fn a_subscribe_that_cannot_be_sent_tears_down_with_a_coded_warning() {
    let t = Instant::now();
    let mut s = ConnectionSupervisor::new(slot_at(DhanEndpointType::MainFeed, COV_SLOT), t);
    let _ = s.on_event(ConnEvent::BeginDial, t);
    let _ = s.on_event(ConnEvent::DialSucceeded, t);
    let c = capture(|| s.on_event(ConnEvent::SubscribeFailed, t));
    assert!(is_redial(&c.value), "{:?}", c.value);
    assert!(c.logs.contains("WARN"));
    assert!(
        c.logs
            .contains(ErrorCode::WsGapSubscriptionBatching.code_str())
    );
    assert!(c.logs.contains("subscribe batch could not be sent"));
}

#[test]
fn an_806_parks_fatally_with_a_coded_error_and_the_park_counter() {
    let t = Instant::now();
    let mut s = live(DhanEndpointType::MainFeed, COV_SLOT, t);
    let c = capture(|| {
        s.on_event(
            ConnEvent::Disconnected {
                code: Some(DisconnectCode::DataApiSubscriptionRequired),
            },
            t,
        )
    });
    assert_eq!(
        c.value,
        SupervisorAction::Park {
            reason: ParkReason::FatalDisconnect
        }
    );
    assert!(c.logs.contains("disconnect_code=806"), "{}", c.logs);
    assert!(c.logs.contains("PARKED PERMANENTLY"));
    assert_eq!(
        count(
            &c.counters,
            &format!(
                "{PARK_METRIC},endpoint=main_feed,reason={}",
                ParkReason::FatalDisconnect.as_str()
            )
        ),
        1,
        "{:?}",
        c.counters
    );
}

#[test]
fn an_804_spends_the_one_respawn_then_parks_on_the_second() {
    let t = Instant::now();
    let mut s = live(DhanEndpointType::Depth200, COV_SLOT, t);
    let close_804 = ConnEvent::Disconnected {
        code: Some(DisconnectCode::InstrumentsExceedLimit),
    };
    let first = capture(|| s.on_event(close_804, t));
    assert!(is_redial(&first.value), "{:?}", first.value);
    assert!(first.logs.contains("disconnect_code=804"), "{}", first.logs);
    assert!(first.logs.contains("worth ONE re-dial"));
    assert_eq!(
        count(
            &first.counters,
            &format!(
                "{RESPAWN_METRIC},endpoint=depth_200,reason={}",
                ParkReason::SubscriptionRejected.as_str()
            )
        ),
        1,
        "{:?}",
        first.counters
    );
    let later = t + Duration::from_secs(60);
    let _ = s.on_event(ConnEvent::BeginDial, later);
    let _ = s.on_event(ConnEvent::DialSucceeded, later);
    let _ = s.on_event(ConnEvent::SubscribeAcked, later);
    let second = capture(|| s.on_event(close_804, later));
    assert_eq!(
        second.value,
        SupervisorAction::Park {
            reason: ParkReason::SubscriptionRejected
        }
    );
    assert!(second.logs.contains("PARKED PERMANENTLY"));
    assert_eq!(
        count(
            &second.counters,
            &format!(
                "{RESPAWN_METRIC},endpoint=depth_200,reason={}",
                ParkReason::SubscriptionRejected.as_str()
            )
        ),
        0
    );
}

#[test]
fn an_idle_live_socket_reconnects_with_a_coded_warning() {
    let t = Instant::now();
    let mut s = live(DhanEndpointType::MainFeed, COV_SLOT, t);
    let before = s.reconnects();
    let c = capture(|| s.on_event(ConnEvent::IdleElapsed, t + Duration::from_secs(30)));
    assert!(is_redial(&c.value), "{:?}", c.value);
    assert_eq!(s.reconnects(), before + 1);
    assert!(c.logs.contains(ErrorCode::WsGapConnectionState.code_str()));
    assert!(c.logs.contains("phase=\"live\""), "{}", c.logs);
}

#[test]
fn a_probe_close_only_closes_a_live_socket() {
    let t = Instant::now();
    let mut idle = ConnectionSupervisor::new(slot_at(DhanEndpointType::Depth200, COV_SLOT), t);
    assert_eq!(
        idle.on_event(ConnEvent::ProbeCloseRequested, t),
        SupervisorAction::Continue
    );
    let mut s = live(DhanEndpointType::Depth200, COV_SLOT, t);
    let before = s.reconnects();
    let c = capture(|| s.on_event(ConnEvent::ProbeCloseRequested, t));
    assert!(is_redial(&c.value), "{:?}", c.value);
    assert_eq!(s.last_redial_reason(), ReconnectReason::ProbeClose);
    assert_eq!(s.reconnects(), before, "a probe close is not a fault");
    assert!(c.logs.contains("source=\"probe_close\""), "{}", c.logs);
}

#[test]
fn poll_of_a_socket_still_subscribing_never_reads_the_register() {
    let t = Instant::now();
    let mut s = ConnectionSupervisor::new(slot_at(DhanEndpointType::MainFeed, COV_SLOT), t);
    let _ = s.on_event(ConnEvent::BeginDial, t);
    let _ = s.on_event(ConnEvent::DialSucceeded, t);
    assert_eq!(s.phase(), ConnPhase::Subscribing);
    let read = std::cell::Cell::new(false);
    let spacing = spacing0();
    let action = s.poll_with(t + Duration::from_secs(1), || {
        read.set(true);
        SilenceInputs {
            wall_ms: 0,
            overflow_or_halted: false,
            stamps: [0; GHOST_REDIAL_SLOTS],
            spacing: &spacing,
        }
    });
    assert_eq!(action, SupervisorAction::Continue);
    assert!(!read.get());
}

#[test]
fn the_300_s_fallback_redials_with_a_coded_frame_silence_warning() {
    let t = Instant::now();
    let mut s = live(DhanEndpointType::MainFeed, COV_SLOT, t);
    let at = t + Duration::from_secs(FRAME_SILENCE_REDIAL_SECS);
    let _ = s.on_event(ConnEvent::KeepAliveReceived, at);
    let spacing = spacing0();
    let c = capture(|| {
        s.poll_with(at, || SilenceInputs {
            wall_ms: 0,
            overflow_or_halted: false,
            stamps: [0; GHOST_REDIAL_SLOTS],
            spacing: &spacing,
        })
    });
    assert!(is_redial(&c.value), "{:?}", c.value);
    assert_eq!(s.last_redial_reason(), ReconnectReason::IdleSilence);
    assert!(c.logs.contains("source=\"frame_silence\""), "{}", c.logs);
    assert!(c.logs.contains("basis=\"unconfirmed\""), "{}", c.logs);
    assert!(
        c.logs
            .contains(&format!("threshold_secs={FRAME_SILENCE_REDIAL_SECS}")),
        "{}",
        c.logs
    );
}

#[test]
fn shadow_mode_counts_and_logs_a_would_redial_once() {
    let t = Instant::now();
    let mut s = ConnectionSupervisor::new(slot_at(DhanEndpointType::MainFeed, 0), t);
    s.set_silence_evidence(SiblingEvidence::Pool);
    s.set_fast_path(FrameSilenceFastPath::Shadow);
    s.set_held_instruments(MAIN_FEED_CONFIRMED_MIN_HELD);
    let _ = s.on_event(ConnEvent::BeginDial, t);
    let _ = s.on_event(ConnEvent::DialSucceeded, t);
    let _ = s.on_event(ConnEvent::SubscribeAcked, t);
    let spacing = spacing0();
    let wall = 1_800_000_000_000_i64;
    let mut stamps = [0_i64; GHOST_REDIAL_SLOTS];
    stamps[1] = wall;
    stamps[2] = wall;
    let threshold = s.confirmed_threshold_secs();
    let at = t + Duration::from_secs(threshold);
    let _ = s.on_event(ConnEvent::KeepAliveReceived, at);
    let c = capture(|| {
        s.poll_with(at, || SilenceInputs {
            wall_ms: wall,
            overflow_or_halted: false,
            stamps,
            spacing: &spacing,
        })
    });
    assert_eq!(c.value, SupervisorAction::Continue, "shadow never redials");
    assert_eq!(s.shadow_would_redials(), 1);
    assert_eq!(
        count(
            &c.counters,
            &format!(
                "{FRAME_SILENCE_WOULD_REDIAL_METRIC},endpoint=main_feed,basis={}",
                SilenceBasis::SiblingConfirmed.as_str()
            )
        ),
        1,
        "{:?}",
        c.counters
    );
    assert!(c.logs.contains("frame_silence_shadow"), "{}", c.logs);
    assert!(c.logs.contains("live_siblings=2"), "{}", c.logs);
    assert_eq!(
        spacing[0].load(std::sync::atomic::Ordering::Relaxed),
        0,
        "shadow takes no spacing slot"
    );
}

// ---------------------------------------------------------------------------
// 808: refresh once, then park (the log stage names)
// ---------------------------------------------------------------------------

fn dial_no_frames(s: &mut ConnectionSupervisor, now: Instant) {
    let _ = s.on_event(ConnEvent::BeginDial, now);
    let _ = s.on_event(ConnEvent::DialSucceeded, now);
}

const CLOSE_808: ConnEvent = ConnEvent::Disconnected {
    code: Some(DisconnectCode::AuthenticationFailed),
};

fn report(presented: u64, outcome: TokenRefreshOutcome) -> TokenRefreshReport {
    TokenRefreshReport { presented, outcome }
}

const PARKED_FATAL: SupervisorAction = SupervisorAction::Park {
    reason: ParkReason::FatalDisconnect,
};

#[test]
fn a_first_808_logs_refresh_then_redial() {
    let t = Instant::now();
    let mut s = ConnectionSupervisor::new(slot_at(DhanEndpointType::MainFeed, 0), t);
    dial_no_frames(&mut s, t);
    let c = capture(|| s.on_event(CLOSE_808, t));
    assert_eq!(
        c.value,
        SupervisorAction::RefreshTokenThenDial {
            delay_ms: TOKEN_STALE_REDIAL_FLOOR_MS
        }
    );
    assert!(
        c.logs.contains("stage=\"refresh_then_redial\""),
        "{}",
        c.logs
    );
    assert!(c.logs.contains("disconnect_code=808"));
}

#[test]
fn a_fresh_token_rejected_again_parks_with_its_stage() {
    let t = Instant::now();
    let mut s = ConnectionSupervisor::new(slot_at(DhanEndpointType::MainFeed, 0), t);
    dial_no_frames(&mut s, t);
    let _ = s.on_event(CLOSE_808, t);
    assert_eq!(
        s.note_token_refresh(report(0, TokenRefreshOutcome::Fresh { current: 1 }), t),
        None
    );
    let later = t + Duration::from_secs(6);
    dial_no_frames(&mut s, later);
    let _ = s.on_event(CLOSE_808, later);
    let c = capture(|| s.note_token_refresh(report(1, TokenRefreshOutcome::NotAttempted), later));
    assert_eq!(c.value, Some(PARKED_FATAL));
    assert!(
        c.logs.contains("stage=\"parked_fresh_token_rejected\""),
        "{}",
        c.logs
    );
}

#[test]
fn auth_reject_hard_failures_park_with_the_refresh_unavailable_stage() {
    let t = Instant::now();
    let mut s = ConnectionSupervisor::new(slot_at(DhanEndpointType::MainFeed, 0), t);
    let mut at = t;
    for _ in 0..AUTH_REJECT_MAX_HARD_FAILURES {
        dial_no_frames(&mut s, at);
        let _ = s.on_event(CLOSE_808, at);
        assert_eq!(
            s.note_token_refresh(report(0, TokenRefreshOutcome::Failed), at),
            None
        );
        at += Duration::from_secs(10);
    }
    dial_no_frames(&mut s, at);
    let c = capture(|| s.on_event(CLOSE_808, at));
    assert_eq!(c.value, PARKED_FATAL);
    assert!(
        c.logs.contains("stage=\"parked_refresh_unavailable\""),
        "{}",
        c.logs
    );
    assert!(
        c.logs
            .contains(&format!("hard_failures={AUTH_REJECT_MAX_HARD_FAILURES}")),
        "{}",
        c.logs
    );
}

#[test]
fn auth_reject_refresh_ceiling_parks_with_its_stage() {
    let t = Instant::now();
    let mut s = ConnectionSupervisor::new(slot_at(DhanEndpointType::MainFeed, 0), t);
    let mut generation = 10_u64;
    let mut at = t;
    for _ in 0..AUTH_REJECT_MAX_REFRESHES_PER_WINDOW {
        dial_no_frames(&mut s, at);
        let _ = s.on_event(CLOSE_808, at);
        let _ = s.note_token_refresh(
            report(
                generation,
                TokenRefreshOutcome::Fresh {
                    current: generation + 1,
                },
            ),
            at,
        );
        generation += 2;
        at += Duration::from_secs(10);
    }
    dial_no_frames(&mut s, at);
    let c = capture(|| s.on_event(CLOSE_808, at));
    assert_eq!(c.value, PARKED_FATAL);
    assert!(
        c.logs.contains("stage=\"parked_refresh_ceiling\""),
        "{}",
        c.logs
    );
}

// ---------------------------------------------------------------------------
// Subscribe guard refusals (logs and counters)
// ---------------------------------------------------------------------------

#[test]
fn a_set_with_repeats_is_deduped_counted_and_names_the_first_repeat() {
    let set = vec![inst(1), inst(2), inst(1), inst(2), inst(3)];
    let c = capture(|| SubscribeGuard::try_new(DhanEndpointType::MainFeed, set));
    let guard = c.value.expect("inside cap");
    assert_eq!(guard.len(), 3);
    assert_eq!(
        count(
            &c.counters,
            &format!("{SUBSCRIBE_DUPLICATE_METRIC},endpoint=main_feed,site=new")
        ),
        2
    );
    assert!(c.logs.contains("security_id=1"), "{}", c.logs);
    assert!(c.logs.contains("dropped=2"), "{}", c.logs);
}

#[test]
fn a_top_up_past_the_cap_is_refused_and_leaves_the_guard_unchanged() {
    let mut guard =
        SubscribeGuard::try_new(DhanEndpointType::Depth20, insts(0..50)).expect("at cap");
    let c = capture(|| guard.try_extend(vec![inst(500)]));
    assert!(matches!(
        c.value,
        Err(SubscribeGuardRefusal::TooManyInstruments { .. })
    ));
    assert_eq!(guard.len(), 50);
    assert!(!guard.holds(inst(500)));
    assert!(c.logs.contains("already=50"), "{}", c.logs);
    assert!(c.logs.contains("adding=1"), "{}", c.logs);
}

#[test]
fn swap_refusals_name_why_and_change_nothing() {
    let mut one = SubscribeGuard::try_new(DhanEndpointType::Depth200, vec![inst(1)]).expect("1");
    let c = capture(|| one.try_swap(inst(2), inst(3)));
    assert!(matches!(
        c.value,
        Err(SubscribeGuardRefusal::NotSubscribed { .. })
    ));
    assert!(c.logs.contains("does not hold"), "{}", c.logs);
    assert_eq!(one.first(), Some(inst(1)));

    let mut two =
        SubscribeGuard::try_new(DhanEndpointType::Depth20, vec![inst(1), inst(2)]).expect("2");
    let c = capture(|| two.try_swap(inst(1), inst(2)));
    assert!(matches!(
        c.value,
        Err(SubscribeGuardRefusal::AlreadySubscribed { .. })
    ));
    assert_eq!(
        count(
            &c.counters,
            &format!("{SUBSCRIBE_DUPLICATE_METRIC},endpoint=depth_20,site=swap")
        ),
        1
    );
    assert!(c.logs.contains("security_id=2"), "{}", c.logs);
    assert!(two.holds(inst(1)) && two.holds(inst(2)));
    assert_eq!(two.len(), 2);
}

#[test]
fn undo_swap_of_an_instrument_not_held_changes_nothing() {
    let mut guard = SubscribeGuard::try_new(DhanEndpointType::Depth20, vec![inst(1), inst(2)])
        .expect("inside cap");
    assert!(!guard.undo_swap(inst(9), inst(8)));
    assert_eq!(guard.len(), 2);
    assert!(guard.holds(inst(1)) && guard.holds(inst(2)) && !guard.holds(inst(8)));
}

#[test]
fn resubscribe_refusals_leave_the_guard_unchanged() {
    let mut guard =
        SubscribeGuard::try_new(DhanEndpointType::Depth20, insts(0..50)).expect("at cap");
    let c = capture(|| guard.try_resubscribe(&[inst(900)], vec![inst(901)]));
    assert!(matches!(
        c.value,
        Err(SubscribeGuardRefusal::NotSubscribed { .. })
    ));
    assert!(c.logs.contains("security_id=900"), "{}", c.logs);
    let c = capture(|| guard.try_resubscribe(&[inst(0)], vec![inst(901), inst(902)]));
    assert!(matches!(
        c.value,
        Err(SubscribeGuardRefusal::TooManyInstruments { .. })
    ));
    assert!(c.logs.contains("removing=1"), "{}", c.logs);
    assert!(c.logs.contains("adding=2"), "{}", c.logs);
    assert_eq!(guard.len(), 50);
    assert!(guard.holds(inst(0)) && !guard.holds(inst(901)));
}

// ---------------------------------------------------------------------------
// Answer helpers: a caller that stopped waiting is not an error
// ---------------------------------------------------------------------------

#[test]
fn answers_reach_a_waiting_caller_and_tolerate_one_that_left() {
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    answer_swap(Some(tx), SwapOutcome::Held, None);
    assert_eq!(rx.try_recv().ok(), Some(SwapOutcome::Held));
    let (tx, rx) = tokio::sync::oneshot::channel::<SwapOutcome>();
    drop(rx);
    answer_swap(
        Some(tx),
        SwapOutcome::NotHeld {
            reason: SwapOutcome::REASON_REFUSED,
        },
        None,
    );

    let (tx, mut rx) = tokio::sync::oneshot::channel();
    answer_probe_unsubscribe(Some(tx), ProbeUnsubscribeOutcome::Dropped);
    assert_eq!(rx.try_recv().ok(), Some(ProbeUnsubscribeOutcome::Dropped));
    let (tx, rx) = tokio::sync::oneshot::channel::<ProbeUnsubscribeOutcome>();
    drop(rx);
    answer_probe_unsubscribe(Some(tx), ProbeUnsubscribeOutcome::Dropped);

    let (tx, rx) = tokio::sync::oneshot::channel::<ResubscribeOutcome>();
    drop(rx);
    answer_resubscribe(Some(tx), ResubscribeOutcome::Applied);
    answer_resubscribe(None, ResubscribeOutcome::Applied);
}

#[test]
fn a_write_reply_with_no_waiting_ticket_is_dropped_quietly() {
    let (ticket, reply) = WriteTicket::pending(7);
    assert_eq!(ticket.generation(), 7);
    drop(ticket);
    reply.send(Ok(()));
    let (ticket, reply) = WriteTicket::pending(8);
    reply.send(Err(SocketFailure));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    assert_eq!(runtime.block_on(ticket), Err(SocketFailure));
}

// ---------------------------------------------------------------------------
// The in-place resubscribe engine over scripted sockets
// ---------------------------------------------------------------------------

struct NullSink;

impl FrameSink for NullSink {
    fn accept(&self, _frame: Bytes) -> FrameSinkOutcome {
        FrameSinkOutcome::Captured
    }
}

/// A transport that never overrides `send_unsubscribe`: the trait default
/// must refuse every unsubscribe.
struct NoUnsubscribeSocket {
    subscribes: usize,
}

impl DhanFeedSocket for NoUnsubscribeSocket {
    fn connect(&mut self) -> impl std::future::Future<Output = Result<(), SocketFailure>> + Send {
        std::future::ready(Ok(()))
    }
    fn write_generation(&self) -> u64 {
        0
    }
    fn send_subscribe(&mut self, _batch: &[SubscribeInstrument]) -> WriteTicket {
        self.subscribes += 1;
        WriteTicket::resolved(0, Ok(()))
    }
    fn send_ping(&mut self) -> WriteTicket {
        WriteTicket::resolved(0, Ok(()))
    }
    fn recv(&mut self) -> impl std::future::Future<Output = SocketEvent> + Send {
        std::future::pending()
    }
    fn close<F>(
        &mut self,
        _on_frame: F,
    ) -> impl std::future::Future<Output = Option<DisconnectCode>> + Send
    where
        F: FnMut(Bytes) + Send,
    {
        std::future::ready(None)
    }
    fn take_owed_close(&mut self) -> Option<DisconnectCode> {
        None
    }
}

/// How the scripted socket answers one write.
#[derive(Clone, Copy, Debug)]
enum Answer {
    Ok,
    Fail,
    /// Never answered (the reply is held, so the ticket stays pending).
    Never,
    /// Answered `Ok` by a spawned task, which only runs once the drain yields.
    Spawned,
    /// Answered `Ok`, but stamped with an older connection's generation.
    Stale,
}

#[derive(Default)]
struct ScriptedSocket {
    unsub: VecDeque<Answer>,
    sub: VecDeque<Answer>,
    /// Events handed out by `recv`; when empty, `recv` never resolves.
    events: VecDeque<SocketEvent>,
    /// Hold every event until a subscribe has been submitted.
    events_after_subscribe: bool,
    subscribes_sent: usize,
    unsubscribes_sent: usize,
    recv_calls: usize,
    held_replies: Vec<WriteReply>,
}

impl ScriptedSocket {
    fn answer(&mut self, answer: Answer) -> WriteTicket {
        match answer {
            Answer::Ok => WriteTicket::resolved(1, Ok(())),
            Answer::Fail => WriteTicket::resolved(1, Err(SocketFailure)),
            Answer::Stale => WriteTicket::resolved(0, Ok(())),
            Answer::Never => {
                let (ticket, reply) = WriteTicket::pending(1);
                self.held_replies.push(reply);
                ticket
            }
            Answer::Spawned => {
                let (ticket, reply) = WriteTicket::pending(1);
                drop(tokio::spawn(async move { reply.send(Ok(())) }));
                ticket
            }
        }
    }
}

impl DhanFeedSocket for ScriptedSocket {
    fn connect(&mut self) -> impl std::future::Future<Output = Result<(), SocketFailure>> + Send {
        std::future::ready(Ok(()))
    }
    fn write_generation(&self) -> u64 {
        1
    }
    fn send_subscribe(&mut self, _batch: &[SubscribeInstrument]) -> WriteTicket {
        self.subscribes_sent += 1;
        let a = self.sub.pop_front().unwrap_or(Answer::Ok);
        self.answer(a)
    }
    fn send_unsubscribe(&mut self, _batch: &[SubscribeInstrument]) -> WriteTicket {
        self.unsubscribes_sent += 1;
        let a = self.unsub.pop_front().unwrap_or(Answer::Ok);
        self.answer(a)
    }
    fn send_ping(&mut self) -> WriteTicket {
        WriteTicket::resolved(1, Ok(()))
    }
    fn recv(&mut self) -> impl std::future::Future<Output = SocketEvent> + Send {
        self.recv_calls += 1;
        let gated = self.events_after_subscribe && self.subscribes_sent == 0;
        let next = if gated { None } else { self.events.pop_front() };
        async move {
            match next {
                Some(event) => event,
                None => std::future::pending().await,
            }
        }
    }
    fn close<F>(
        &mut self,
        _on_frame: F,
    ) -> impl std::future::Future<Output = Option<DisconnectCode>> + Send
    where
        F: FnMut(Bytes) + Send,
    {
        std::future::ready(None)
    }
    fn take_owed_close(&mut self) -> Option<DisconnectCode> {
        None
    }
}

/// One `apply_resubscribe` on a live main-feed connection at [`COV_SLOT`].
async fn run_resubscribe<S: DhanFeedSocket>(
    socket: &mut S,
    held: Vec<SubscribeInstrument>,
    unsubscribe: Vec<SubscribeInstrument>,
    subscribe: Vec<SubscribeInstrument>,
) -> (SupervisorAction, Option<ResubscribeOutcome>, SubscribeGuard) {
    let t = Instant::now();
    let mut supervisor = live(DhanEndpointType::MainFeed, COV_SLOT, t);
    let mut guard = SubscribeGuard::try_new(DhanEndpointType::MainFeed, held).expect("inside cap");
    let mut throttle = DrainThrottle::default();
    let (ack, mut rx) = tokio::sync::oneshot::channel();
    let action = apply_resubscribe(
        socket,
        &mut supervisor,
        &NullSink,
        &mut throttle,
        &mut guard,
        unsubscribe,
        subscribe,
        Some(ack),
    )
    .await;
    (action, rx.try_recv().ok(), guard)
}

fn inplace(outcome: &str) -> String {
    format!("{INPLACE_CHANGE_METRIC},endpoint=main_feed,outcome={outcome}")
}

#[test]
fn a_transport_that_cannot_unsubscribe_stops_the_change_before_any_subscribe() {
    let c = capture_async(|| async {
        let mut socket = NoUnsubscribeSocket { subscribes: 0 };
        let out = run_resubscribe(&mut socket, insts(1..4), vec![inst(1)], vec![inst(10)]).await;
        (out, socket.subscribes)
    });
    let ((action, ack, guard), subscribes) = c.value;
    assert_eq!(action, SupervisorAction::Continue, "the socket stays up");
    assert_eq!(subscribes, 0, "never subscribe after a failed unsubscribe");
    assert_eq!(
        ack,
        Some(ResubscribeOutcome::Stopped {
            not_unsubscribed: vec![inst(1)],
            not_subscribed: vec![inst(10)],
        })
    );
    assert_eq!(guard.len(), 3);
    assert!(guard.holds(inst(1)) && !guard.holds(inst(10)));
    assert_eq!(count(&c.counters, &inplace("stopped")), 1);
    assert!(c.logs.contains("inplace_unsubscribe_stopped"), "{}", c.logs);
}

#[test]
fn refused_resubscribes_send_nothing_and_say_why() {
    let c = capture_async(|| async {
        let mut socket = ScriptedSocket::default();
        let over =
            run_resubscribe(&mut socket, insts(0..5_000), Vec::new(), vec![inst(9_999)]).await;
        let not_held = run_resubscribe(&mut socket, insts(0..3), vec![inst(77)], Vec::new()).await;
        (
            over,
            not_held,
            socket.subscribes_sent + socket.unsubscribes_sent,
        )
    });
    let ((a1, ack1, g1), (a2, ack2, g2), writes) = c.value;
    assert_eq!(writes, 0);
    assert_eq!(a1, SupervisorAction::Continue);
    assert_eq!(a2, SupervisorAction::Continue);
    assert_eq!(
        ack1,
        Some(ResubscribeOutcome::Refused {
            reason: ResubscribeOutcome::REASON_OVER_CAP
        })
    );
    assert_eq!(
        ack2,
        Some(ResubscribeOutcome::Refused {
            reason: ResubscribeOutcome::REASON_NOT_HELD
        })
    );
    assert_eq!(g1.len(), 5_000);
    assert_eq!(g2.len(), 3);
    assert_eq!(count(&c.counters, &inplace("refused_over_cap")), 1);
    assert_eq!(count(&c.counters, &inplace("refused_not_held")), 1);
    assert!(c.logs.contains("REFUSED before anything was sent"));
}

#[test]
fn a_close_during_the_unsubscribe_hands_the_socket_decision_back() {
    let c = capture_async(|| async {
        let mut socket = ScriptedSocket {
            unsub: VecDeque::from([Answer::Never]),
            events: VecDeque::from([SocketEvent::Closed {
                code: Some(DisconnectCode::InternalServerError),
            }]),
            ..ScriptedSocket::default()
        };
        let out = run_resubscribe(&mut socket, insts(1..4), vec![inst(1)], vec![inst(10)]).await;
        (out, socket.subscribes_sent)
    });
    let ((action, ack, guard), subscribes) = c.value;
    assert!(is_redial(&action), "{action:?}");
    assert_eq!(ack, Some(ResubscribeOutcome::ReplayPending));
    assert_eq!(subscribes, 0);
    assert!(guard.holds(inst(10)), "the replay sends the requested set");
    assert_eq!(count(&c.counters, &inplace("replay_pending")), 1);
}

#[test]
fn an_unsubscribe_that_never_answers_is_counted_landed_and_stops_the_change() {
    let c = capture_async(|| async {
        let mut socket = ScriptedSocket {
            unsub: VecDeque::from([Answer::Never]),
            ..ScriptedSocket::default()
        };
        let out = run_resubscribe(&mut socket, insts(1..4), vec![inst(1)], vec![inst(10)]).await;
        (out, socket.subscribes_sent)
    });
    let ((action, ack, guard), subscribes) = c.value;
    assert_eq!(action, SupervisorAction::Continue);
    assert_eq!(subscribes, 0);
    assert_eq!(
        ack,
        Some(ResubscribeOutcome::Stopped {
            not_unsubscribed: Vec::new(),
            not_subscribed: vec![inst(10)],
        })
    );
    assert_eq!(guard.len(), 2, "the timed-out unsubscribe counts as landed");
    assert!(!guard.holds(inst(1)) && !guard.holds(inst(10)));
}

#[test]
fn a_change_that_empties_the_socket_redials_to_replay_the_requested_set() {
    let c = capture_async(|| async {
        let mut socket = ScriptedSocket {
            sub: VecDeque::from([Answer::Fail]),
            ..ScriptedSocket::default()
        };
        run_resubscribe(&mut socket, insts(1..3), insts(1..3), vec![inst(10)]).await
    });
    let (action, ack, guard) = c.value;
    assert!(is_redial(&action), "{action:?}");
    assert_eq!(ack, Some(ResubscribeOutcome::ReplayPending));
    assert_eq!(guard.len(), 1);
    assert!(guard.holds(inst(10)));
    assert_eq!(count(&c.counters, &inplace("emptied_redial")), 1);
    assert!(c.logs.contains("inplace_emptied_socket"), "{}", c.logs);
}

#[test]
fn a_failed_subscribe_after_a_landed_unsubscribe_stops_and_returns_the_rest() {
    let c = capture_async(|| async {
        let mut socket = ScriptedSocket {
            sub: VecDeque::from([Answer::Fail]),
            ..ScriptedSocket::default()
        };
        run_resubscribe(
            &mut socket,
            insts(1..3),
            vec![inst(1)],
            vec![inst(10), inst(11)],
        )
        .await
    });
    let (action, ack, guard) = c.value;
    assert_eq!(action, SupervisorAction::Continue);
    assert_eq!(
        ack,
        Some(ResubscribeOutcome::Stopped {
            not_unsubscribed: Vec::new(),
            not_subscribed: vec![inst(10), inst(11)],
        })
    );
    assert_eq!(guard.len(), 1);
    assert!(guard.holds(inst(2)));
    assert_eq!(count(&c.counters, &inplace("stopped")), 1);
    assert!(c.logs.contains("inplace_subscribe_stopped"), "{}", c.logs);
}

#[test]
fn a_close_during_the_subscribe_hands_the_socket_decision_back() {
    let c = capture_async(|| async {
        let mut socket = ScriptedSocket {
            sub: VecDeque::from([Answer::Never]),
            events: VecDeque::from([SocketEvent::Closed {
                code: Some(DisconnectCode::InternalServerError),
            }]),
            events_after_subscribe: true,
            ..ScriptedSocket::default()
        };
        let out = run_resubscribe(&mut socket, insts(1..3), vec![inst(1)], vec![inst(10)]).await;
        (out, socket.unsubscribes_sent)
    });
    let ((action, ack, guard), unsubscribes) = c.value;
    assert_eq!(unsubscribes, 1);
    assert!(is_redial(&action), "{action:?}");
    assert_eq!(ack, Some(ResubscribeOutcome::ReplayPending));
    assert!(guard.holds(inst(10)) && !guard.holds(inst(1)));
    assert_eq!(count(&c.counters, &inplace("replay_pending")), 1);
}

#[test]
fn a_stale_write_answer_is_not_trusted_as_landed() {
    let c = capture_async(|| async {
        let mut socket = ScriptedSocket {
            sub: VecDeque::from([Answer::Stale]),
            ..ScriptedSocket::default()
        };
        run_resubscribe(&mut socket, vec![inst(1)], Vec::new(), vec![inst(10)]).await
    });
    let (action, ack, guard) = c.value;
    assert_eq!(action, SupervisorAction::Continue);
    assert_eq!(
        ack,
        Some(ResubscribeOutcome::Stopped {
            not_unsubscribed: Vec::new(),
            not_subscribed: vec![inst(10)],
        })
    );
    assert_eq!(guard.len(), 1);
    assert!(c.logs.contains("stale_write_answer"), "{}", c.logs);
    assert!(c.logs.contains("issued_generation=0"), "{}", c.logs);
    assert!(c.logs.contains("current_generation=1"), "{}", c.logs);
}

#[test]
fn a_reader_that_never_pauses_yields_so_the_writer_can_answer() {
    let c = capture_async(|| async {
        let keepalives = TOPUP_GAP_MAX_EVENTS * 2;
        let mut socket = ScriptedSocket {
            sub: VecDeque::from([Answer::Spawned]),
            events: (0..keepalives).map(|_| SocketEvent::KeepAlive).collect(),
            ..ScriptedSocket::default()
        };
        let out = run_resubscribe(&mut socket, vec![inst(1)], vec![inst(1)], vec![inst(10)]).await;
        (out, socket.recv_calls)
    });
    let ((action, ack, guard), recv_calls) = c.value;
    assert_eq!(action, SupervisorAction::Continue);
    assert_eq!(ack, Some(ResubscribeOutcome::Applied));
    assert!(
        recv_calls >= TOPUP_GAP_MAX_EVENTS,
        "the answer only arrives once the reader yields ({recv_calls} reads)"
    );
    assert_eq!(guard.len(), 1);
    assert!(guard.holds(inst(10)));
    assert_eq!(count(&c.counters, &inplace("applied")), 1);
    assert!(
        c.logs.contains("in-place resubscribe applied"),
        "{}",
        c.logs
    );
    assert!(c.logs.contains("unsubscribed=1"), "{}", c.logs);
}

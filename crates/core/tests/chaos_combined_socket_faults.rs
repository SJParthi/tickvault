//! Combined-fault chaos: the 808 refresh-once state machine under a whole
//! pool of sockets rejected at the 09:15 open (scenario S7).
//!
//! # Why this file exists
//!
//! The inline tests in `pool_supervisor.rs` drive ONE `ConnectionSupervisor`
//! through the 808 rules (refresh once, park on (a) the verified generation
//! rejected again, (b) 3 outright refresh failures, (c) 300 s without a fresh
//! token, (d) more than 4 fresh tokens in 300 s), and one test checks that
//! sixteen first 808s fan out above the 5 s floor. Nothing drove sixteen
//! supervisors THROUGH THE WHOLE EPISODE TOGETHER against one shared token,
//! which is what the open looks like when every socket is rejected at once:
//! the refresh is shared (a socket whose dialled generation was already
//! replaced by a sibling reports `Fresh` without minting; a renewal failure is
//! reused for 10 s as a deferral, `RENEW_FAILURE_REUSE_MS`), so one socket's
//! outcome is another socket's input.
//!
//! # The model
//!
//! Every supervisor is the REAL `ConnectionSupervisor` (public API only:
//! `on_event`, `token_refresh_request`, `note_token_refresh`). The shell is
//! replaced by an event-driven loop that does what `run_connection_with_commands`
//! does for `RefreshTokenThenDial` (sleep the returned delay, refresh, report,
//! then `BeginDial`), on simulated `Instant`s, so 3,600 s of open run in
//! milliseconds. The token is [`SharedToken`], a TEST MODEL of the app's
//! refresh closure (`dhan_feed_stack.rs`) plus the token manager's 10 s
//! failure reuse: skip when the request says so (`NotAttempted`), `Fresh`
//! when the generation already moved past the dialled one, a reused failure
//! inside 10 s is `Deferred`, otherwise one real mint whose result depends on
//! the scenario. The vendor is a predicate on the presented generation.
//!
//! Honest limits:
//! - The mint cooldown is modelled only as "every real attempt defers"
//!   (scenario D); the real token manager's cooldown arithmetic is not
//!   reproduced here.
//! - The 10 s failure reuse lives in the MODEL, so nothing in this file can
//!   prove the real `TokenManager` reuses a failure for 10 s: its test
//!   constructor is `#[cfg(test)] pub(crate)` and unreachable from an
//!   integration test. That is covered by the `token_manager.rs` unit tests,
//!   not here. Assertions on mint counts (`token.mints`) are partly
//!   model-level too: the model never mints once the generation moved past
//!   the dialled one. Only the assertions on supervisor ACTIONS (delays,
//!   refresh requests, parks, rule tags) test the real code.
//! - A sibling mint landing between one socket's refresh and its dial (the
//!   dial takes a TLS handshake in production) is modelled by an `between`
//!   hook that moves the generation as a sibling would; scenario F caps those
//!   external mints so the model cannot invent an unbounded minter.
//! - An 805 park in the middle of an 808 episode is not driven here.
//!
//! # Scenarios
//!
//! - **A** stale token at the open, mint works: one shared mint, every socket
//!   asked for exactly one refresh, all sixteen come back, none parks.
//! - **B** the account itself is refused (a wrong client id answered as 808):
//!   one shared mint, every socket parks on rule (a) at its second 808, no
//!   second mint.
//! - **C** the mint endpoint fails outright: every refresh redial waits at
//!   least the 5 s token floor; no socket parks except on rule (b) or rule (c);
//!   every socket parks in bounded time.
//! - **D** every mint is deferred (cooldown): no socket parks before 300 s
//!   from its episode start; every socket parks within 300 s plus three
//!   redial cycles.
//! - **E** the mint fails for the first 60 s, then works: every socket that
//!   did not reach 3 outright failures comes back; no park outside the rules.
//! - **F** a seeded random mix (vendor accept/reject per dial; accepted
//!   sockets later dropped with 808, 807, 809 or a bare reset, with or without
//!   streaming frames first; mint ok/fail/defer; capped sibling mints between
//!   refresh and dial) over 300 seeds: the oracle below holds at every park,
//!   nothing dials after a park, no slot records more than 4 fresh tokens in
//!   any 300 s window, every socket ends live or parked, the episode resets
//!   are reached, and parks on rules (a), (b), (c) and (d) are each reached.
//! - **G** a socket that streamed for at least `MIN_HEALTHY_SESSION_MS` and
//!   then gets an 808, or streamed and then closed with a bare reset, starts
//!   a new episode with three fresh strikes; under the minimum the old
//!   episode carries on.
//! - **H** a sibling moves the token between this socket's refresh and its
//!   dial every cycle: the socket parks on rule (d) at its 5th 808, not its
//!   4th.
//! - **I** an 807 refresh inside an open 808 episode neither verifies a
//!   generation nor adds a strike.
//! - **C-boundary** rule (c) at exactly 300 s, and the one grace refresh after
//!   a cycle that began without a refresh.
//!
//! The oracle (necessary conditions, read from the source): a park reported by
//! `note_token_refresh` needs the presented generation to equal the one the
//! episode verified (rule a); a park on an 808 close needs rule (d) 4 fresh
//! tokens inside the last 300 s, rule (b) 3 outright failures, or rule (c)
//! 300 s since the episode start or the last fresh token. The oracle ends an
//! episode where the source does: an 808 after at least
//! `MIN_HEALTHY_SESSION_MS` of streaming, or any other close after a frame.
//! Refresh outcomes are booked into the oracle only after an 808 close, as
//! the supervisor ignores them otherwise.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use tickvault_core::auth::token_manager::RENEW_FAILURE_REUSE_MS;
use tickvault_core::websocket::pool_budget::{ConnectionSlot, DhanAccount, DhanEndpointType};
use tickvault_core::websocket::pool_supervisor::{
    AUTH_REJECT_EPISODE_MAX_MS, AUTH_REJECT_MAX_HARD_FAILURES,
    AUTH_REJECT_MAX_REFRESHES_PER_WINDOW, AUTH_REJECT_WINDOW_MS, ConnEvent, ConnPhase,
    ConnectionSupervisor, ParkReason, SupervisorAction, TOKEN_STALE_REDIAL_FLOOR_MS,
    TokenRefreshOutcome, TokenRefreshReport, TokenRefreshRequest,
};
use tickvault_core::websocket::reconnect_ladder::MIN_HEALTHY_SESSION_MS;
use tickvault_core::websocket::types::DisconnectCode;

/// How one real mint attempt ends in a scenario.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mint {
    Ok,
    Fail,
    Defer,
}

/// The shared token: one generation for the whole pool, as in production.
struct SharedToken {
    generation: u64,
    /// The last renewal failure (when, for which dialled generation), reused
    /// as a deferral for 10 s like `RENEW_FAILURE_REUSE_MS`.
    last_failure: Option<(Instant, u64)>,
    /// Successful mints by a socket's refresh.
    mints: u32,
    /// Generation moves made by the `between` hook (a sibling mint landing
    /// between one socket's refresh and its dial).
    external_mints: u32,
}

impl SharedToken {
    fn new() -> Self {
        Self {
            generation: 0,
            last_failure: None,
            mints: 0,
            external_mints: 0,
        }
    }

    /// The app's refresh closure, modelled.
    fn refresh(
        &mut self,
        request: TokenRefreshRequest,
        dialled: u64,
        now: Instant,
        mint: Mint,
    ) -> TokenRefreshReport {
        let report = |outcome| TokenRefreshReport {
            presented: dialled,
            outcome,
        };
        if request.skip_if_presented == Some(dialled) {
            return report(TokenRefreshOutcome::NotAttempted);
        }
        if self.generation != dialled {
            // A sibling already replaced the token this socket dialled with.
            return report(TokenRefreshOutcome::Fresh {
                current: self.generation,
            });
        }
        if let Some((at, generation)) = self.last_failure
            && generation == dialled
            && now.saturating_duration_since(at) < Duration::from_millis(RENEW_FAILURE_REUSE_MS)
        {
            return report(TokenRefreshOutcome::Deferred);
        }
        match mint {
            Mint::Ok => {
                self.generation += 1;
                self.mints += 1;
                self.last_failure = None;
                report(TokenRefreshOutcome::Fresh {
                    current: self.generation,
                })
            }
            Mint::Fail => {
                self.last_failure = Some((now, dialled));
                report(TokenRefreshOutcome::Failed)
            }
            Mint::Defer => {
                self.last_failure = Some((now, dialled));
                report(TokenRefreshOutcome::Deferred)
            }
        }
    }

    /// A sibling's mint landing between this socket's refresh and its dial.
    fn sibling_mint(&mut self) {
        self.generation += 1;
        self.external_mints += 1;
        self.last_failure = None;
    }
}

/// Test-side record of one slot's 808 episode, for the oracle.
#[derive(Default)]
struct Oracle {
    episode_start: Option<Instant>,
    last_progress: Option<Instant>,
    hard_failures: u32,
    verified: Option<u64>,
    /// Fresh tokens charged to this slot. Deliberately NOT cleared when an
    /// episode ends: the supervisor's rule-(d) ring spans episodes.
    fresh: Vec<Instant>,
}

impl Oracle {
    fn fresh_in_window(&self, now: Instant) -> usize {
        let window = Duration::from_millis(AUTH_REJECT_WINDOW_MS);
        self.fresh
            .iter()
            .filter(|t| now.saturating_duration_since(**t) < window)
            .count()
    }

    fn start_episode(&mut self, at: Instant) {
        self.episode_start = Some(at);
        self.last_progress = Some(at);
        self.hard_failures = 0;
        self.verified = None;
    }

    fn end_episode(&mut self) {
        self.episode_start = None;
        self.last_progress = None;
        self.hard_failures = 0;
        self.verified = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fate {
    /// Waiting to wake (dial or refresh-then-dial) at `wake`.
    Waiting,
    /// Accepted by the vendor and left streaming (or waiting for a scheduled
    /// close).
    Live,
    /// Parked.
    Parked,
}

/// Which documented park rule the oracle saw allow a park.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rule {
    A,
    B,
    C,
    D,
}

struct Sock {
    sup: ConnectionSupervisor,
    wake: Instant,
    /// What the shell does when it wakes.
    refresh_first: bool,
    /// Whether the pending refresh follows an 808 close (only then does the
    /// supervisor read its outcome).
    refresh_after_808: bool,
    /// Generation the last dial presented.
    presented: u64,
    /// The current connection's first frame, if it streamed (shell model of
    /// `healthy_since`).
    streaming_since: Option<Instant>,
    fate: Fate,
    parked_at: Option<Instant>,
    park_rule: Option<Rule>,
    oracle: Oracle,
    delays: Vec<u64>,
    /// Delays of the `RefreshTokenThenDial` actions only.
    refresh_delays: Vec<u64>,
    /// Instants this slot ran a token refresh.
    refreshes: Vec<Instant>,
    /// Instants of `Fresh` reports this slot fed back (any episode).
    fresh_log: Vec<Instant>,
    /// Episodes the oracle saw end without a park.
    episode_resets: u32,
}

/// What the vendor does to a dial that presented `generation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    /// 808 on the handshake.
    Reject808,
    /// Accepted and left streaming.
    Accept,
    /// Accepted, then closed `after` later with `code` (`None` = bare reset).
    /// With `stream`, the socket delivers frames from the ack until the close.
    AcceptThen {
        after: Duration,
        code: Option<DisconnectCode>,
        stream: bool,
    },
}

const AUTH_808: Option<DisconnectCode> = Some(DisconnectCode::AuthenticationFailed);

fn all_slots() -> Vec<ConnectionSlot> {
    let mut slots = Vec::new();
    for endpoint in DhanEndpointType::ALL {
        for pool_index in 0..endpoint.max_connections() {
            slots.push(ConnectionSlot {
                account: DhanAccount::Primary,
                endpoint,
                pool_index,
                global_index: endpoint.jitter_base().saturating_add(pool_index),
            });
        }
    }
    slots
}

/// The outcome of one simulated open.
struct Run {
    socks: Vec<Sock>,
    token: SharedToken,
    start: Instant,
    violations: Vec<String>,
}

impl Run {
    fn rule_count(&self, rule: Rule) -> usize {
        self.socks
            .iter()
            .filter(|s| s.park_rule == Some(rule))
            .count()
    }
    fn episode_resets(&self) -> u32 {
        self.socks.iter().map(|s| s.episode_resets).sum()
    }
}

/// Drives every socket from a first dial at `start` (09:15) until each is
/// live or parked, or `horizon` passes. `vendor(slot, presented, now, dial_no)`
/// and `mint(now)` are the scenario; `between(now)` returning true moves the
/// generation (a sibling mint) after a refresh and before the dial.
fn run_open(
    horizon: Duration,
    mut vendor: impl FnMut(usize, u64, Instant, u32) -> Verdict,
    mut mint: impl FnMut(Instant) -> Mint,
    mut between: impl FnMut(Instant) -> bool,
) -> Run {
    let start = Instant::now();
    let mut socks: Vec<Sock> = all_slots()
        .into_iter()
        .map(|slot| Sock {
            sup: ConnectionSupervisor::new(slot, start),
            wake: start,
            refresh_first: false,
            refresh_after_808: false,
            presented: 0,
            streaming_since: None,
            fate: Fate::Waiting,
            parked_at: None,
            park_rule: None,
            oracle: Oracle::default(),
            delays: Vec::new(),
            refresh_delays: Vec::new(),
            refreshes: Vec::new(),
            fresh_log: Vec::new(),
            episode_resets: 0,
        })
        .collect();
    assert_eq!(socks.len(), 16, "the sixteen primary-account slots");
    let mut token = SharedToken::new();
    let mut violations = Vec::new();
    let mut dials = vec![0u32; socks.len()];
    // Pending closes for accepted sockets: (slot, at, code).
    let mut closes: Vec<(usize, Instant, Option<DisconnectCode>)> = Vec::new();
    let end = start + horizon;
    loop {
        // Next event: a socket wake or a scheduled close, earliest first.
        let next_wake = socks
            .iter()
            .enumerate()
            .filter(|(_, s)| s.fate == Fate::Waiting)
            .min_by_key(|(_, s)| s.wake)
            .map(|(i, s)| (i, s.wake));
        let next_close = closes
            .iter()
            .enumerate()
            .min_by_key(|(_, c)| c.1)
            .map(|(k, c)| (k, c.1));
        let (is_close, idx, at) = match (next_wake, next_close) {
            (None, None) => break,
            (Some((i, w)), None) => (false, i, w),
            (None, Some((k, c))) => (true, k, c),
            (Some((i, w)), Some((k, c))) => {
                if c <= w {
                    (true, k, c)
                } else {
                    (false, i, w)
                }
            }
        };
        if at > end {
            break;
        }
        let (i, close_code) = if is_close {
            let (i, _, code) = closes.remove(idx);
            (i, Some(code))
        } else {
            (idx, None)
        };
        let sock = &mut socks[i];
        let (action, after_808) = if let Some(code) = close_code {
            // A previously accepted socket closes now.
            sock.fate = Fate::Waiting;
            if let Some(since) = sock.streaming_since {
                // One more frame just before the close keeps the model honest
                // about a socket that streamed right up to it.
                let _ = sock.sup.on_event(ConnEvent::FrameReceived, at);
                let healthy = at.saturating_duration_since(since);
                if code == AUTH_808 {
                    if sock.oracle.episode_start.is_some()
                        && healthy >= Duration::from_millis(MIN_HEALTHY_SESSION_MS)
                    {
                        sock.oracle.end_episode();
                        sock.episode_resets += 1;
                    }
                } else if sock.oracle.episode_start.is_some() {
                    // Any other close after frames ends the episode.
                    sock.oracle.end_episode();
                    sock.episode_resets += 1;
                }
            }
            if code == AUTH_808 && sock.oracle.episode_start.is_none() {
                sock.oracle.start_episode(at);
            }
            let action = sock.sup.on_event(ConnEvent::Disconnected { code }, at);
            check_close_park(sock, &action, code, at, &mut violations);
            (action, code == AUTH_808)
        } else {
            // Wake: refresh first when the last action asked for it.
            let mut parked = None;
            if sock.refresh_first {
                let request = sock.sup.token_refresh_request();
                if !sock.refresh_after_808 && request.skip_if_presented.is_some() {
                    violations.push(format!(
                        "slot {i}: a refresh after a non-808 close asked to skip {request:?}"
                    ));
                }
                let report = token.refresh(request, sock.presented, at, mint(at));
                sock.refreshes.push(at);
                let verified_before = sock.oracle.verified;
                parked = sock.sup.note_token_refresh(report, at);
                if let Some(p) = &parked {
                    if !matches!(p, SupervisorAction::Park { .. }) {
                        violations.push(format!("slot {i}: a refresh report returned {p:?}"));
                    }
                    if !sock.refresh_after_808 {
                        violations.push(format!(
                            "slot {i}: a refresh after a non-808 close parked: {p:?}"
                        ));
                    }
                    if verified_before != Some(report.presented) {
                        violations.push(format!(
                            "slot {i}: parked on a refresh report although the presented \
                             generation {} was not the verified {:?} (rule a)",
                            report.presented, verified_before
                        ));
                    }
                    sock.park_rule = Some(Rule::A);
                } else if sock.refresh_after_808 {
                    match report.outcome {
                        TokenRefreshOutcome::Fresh { current } => {
                            sock.oracle.verified = Some(current);
                            sock.oracle.last_progress = Some(at);
                            sock.oracle.fresh.push(at);
                            sock.fresh_log.push(at);
                        }
                        TokenRefreshOutcome::Failed | TokenRefreshOutcome::NotAttempted => {
                            sock.oracle.hard_failures += 1;
                        }
                        TokenRefreshOutcome::Deferred => {}
                    }
                }
                if parked.is_none() && between(at) {
                    token.sibling_mint();
                }
            }
            match parked {
                Some(park) => (park, false),
                None => {
                    let dial = sock.sup.on_event(ConnEvent::BeginDial, at);
                    if dial != SupervisorAction::Dial {
                        violations.push(format!("slot {i}: BeginDial returned {dial:?}"));
                        sock.fate = Fate::Parked;
                        continue;
                    }
                    sock.presented = token.generation;
                    sock.streaming_since = None;
                    dials[i] += 1;
                    let _ = sock.sup.on_event(ConnEvent::DialSucceeded, at);
                    match vendor(i, sock.presented, at, dials[i]) {
                        Verdict::Reject808 => {
                            if sock.oracle.episode_start.is_none() {
                                sock.oracle.start_episode(at);
                            }
                            let action = sock
                                .sup
                                .on_event(ConnEvent::Disconnected { code: AUTH_808 }, at);
                            check_close_park(sock, &action, AUTH_808, at, &mut violations);
                            (action, true)
                        }
                        Verdict::Accept => {
                            let _ = sock.sup.on_event(ConnEvent::SubscribeAcked, at);
                            sock.fate = Fate::Live;
                            continue;
                        }
                        Verdict::AcceptThen {
                            after,
                            code,
                            stream,
                        } => {
                            let _ = sock.sup.on_event(ConnEvent::SubscribeAcked, at);
                            if stream {
                                let _ = sock.sup.on_event(ConnEvent::FrameReceived, at);
                                sock.streaming_since = Some(at);
                            }
                            sock.fate = Fate::Live;
                            closes.push((i, at + after, code));
                            continue;
                        }
                    }
                }
            }
        };
        let sock = &mut socks[i];
        match action {
            SupervisorAction::RefreshTokenThenDial { delay_ms } => {
                sock.refresh_first = true;
                sock.refresh_after_808 = after_808;
                sock.wake = at + Duration::from_millis(delay_ms);
                sock.delays.push(delay_ms);
                sock.refresh_delays.push(delay_ms);
                sock.fate = Fate::Waiting;
            }
            SupervisorAction::SleepThenDial { delay_ms } => {
                sock.refresh_first = false;
                sock.refresh_after_808 = false;
                sock.wake = at + Duration::from_millis(delay_ms);
                sock.delays.push(delay_ms);
                sock.fate = Fate::Waiting;
            }
            SupervisorAction::Park { reason } => {
                if reason != ParkReason::FatalDisconnect {
                    violations.push(format!(
                        "slot {i}: parked for {reason:?}, not FatalDisconnect"
                    ));
                }
                sock.fate = Fate::Parked;
                sock.parked_at = Some(at);
                // Nothing dials after a park.
                let again = sock
                    .sup
                    .on_event(ConnEvent::BeginDial, at + Duration::from_secs(1));
                if again == SupervisorAction::Dial {
                    violations.push(format!("slot {i}: dialled after a park"));
                }
                if sock.sup.phase() != ConnPhase::Parked {
                    violations.push(format!(
                        "slot {i}: park action but phase {:?}",
                        sock.sup.phase()
                    ));
                }
            }
            other => {
                violations.push(format!("slot {i}: unexpected action {other:?}"));
                sock.fate = Fate::Parked;
            }
        }
    }
    // Rule (d), checked on the record: never more than 4 fresh tokens charged
    // to one slot inside any 300 s window (only a non-parking Fresh counts).
    for (i, sock) in socks.iter().enumerate() {
        let window = Duration::from_millis(AUTH_REJECT_WINDOW_MS);
        for (k, t) in sock.fresh_log.iter().enumerate() {
            let in_window = sock.fresh_log[k..]
                .iter()
                .filter(|u| u.saturating_duration_since(*t) < window)
                .count();
            if in_window > AUTH_REJECT_MAX_REFRESHES_PER_WINDOW {
                violations.push(format!(
                    "slot {i}: {in_window} fresh tokens inside one 300 s window"
                ));
            }
        }
    }
    Run {
        socks,
        token,
        start,
        violations,
    }
}

/// The oracle for a park on an 808 close: rule (d), (b) or (c) must hold, in
/// the order the source checks them. Tags the park with the first that holds.
fn check_close_park(
    sock: &mut Sock,
    action: &SupervisorAction,
    code: Option<DisconnectCode>,
    at: Instant,
    violations: &mut Vec<String>,
) {
    if !matches!(action, SupervisorAction::Park { .. }) {
        return;
    }
    if code != AUTH_808 {
        violations.push(format!("parked on a non-808 close {code:?}: {action:?}"));
        return;
    }
    let rule_d = sock.oracle.fresh_in_window(at) >= AUTH_REJECT_MAX_REFRESHES_PER_WINDOW;
    let rule_b = sock.oracle.hard_failures >= u32::from(AUTH_REJECT_MAX_HARD_FAILURES);
    let rule_c = sock.oracle.last_progress.is_some_and(|p| {
        at.saturating_duration_since(p) >= Duration::from_millis(AUTH_REJECT_EPISODE_MAX_MS)
    });
    sock.park_rule = if rule_d {
        Some(Rule::D)
    } else if rule_b {
        Some(Rule::B)
    } else if rule_c {
        Some(Rule::C)
    } else {
        violations.push(format!(
            "parked on an 808 with no documented condition: hard {} since_progress {:?} \
             fresh_in_window {}",
            sock.oracle.hard_failures,
            sock.oracle
                .last_progress
                .map(|p| at.saturating_duration_since(p)),
            sock.oracle.fresh_in_window(at)
        ));
        None
    };
}

fn max_delay(run: &Run) -> Duration {
    Duration::from_millis(
        run.socks
            .iter()
            .flat_map(|s| s.delays.iter().copied())
            .max()
            .unwrap_or(0),
    )
}

fn no_sibling(_: Instant) -> bool {
    false
}

const HOUR: Duration = Duration::from_secs(3_600);

/// A: a stale token at the open, the mint works. Every socket asks for
/// exactly one refresh (supervisor-controlled), the pool shares one mint
/// (model-level), all sixteen come back, none parks, and every first redial
/// waits at least the 5 s floor.
#[test]
fn s7a_sixteen_808s_at_the_open_share_one_mint_and_all_recover() {
    let run = run_open(
        HOUR,
        |_, presented, _, _| {
            if presented == 0 {
                Verdict::Reject808
            } else {
                Verdict::Accept
            }
        },
        |_| Mint::Ok,
        no_sibling,
    );
    assert!(run.violations.is_empty(), "{:#?}", run.violations);
    assert_eq!(
        run.token.mints, 1,
        "one shared mint for sixteen 808s (model)"
    );
    for (i, s) in run.socks.iter().enumerate() {
        assert_eq!(s.fate, Fate::Live, "slot {i} came back");
        assert_eq!(
            s.refresh_delays.len(),
            1,
            "slot {i}: one 808, one refresh-then-dial"
        );
        assert!(
            s.delays.first().copied().unwrap_or(0) >= TOKEN_STALE_REDIAL_FLOOR_MS,
            "slot {i} waited the token floor"
        );
    }
}

/// B: the account itself is refused. One mint, then every socket parks on
/// rule (a) at its second 808; nobody mints again.
#[test]
fn s7b_account_level_808_parks_every_socket_on_rule_a_after_one_mint() {
    let run = run_open(
        HOUR,
        |_, _, _, _| Verdict::Reject808,
        |_| Mint::Ok,
        no_sibling,
    );
    assert!(run.violations.is_empty(), "{:#?}", run.violations);
    assert_eq!(run.token.mints, 1, "the verified token is never replaced");
    for (i, s) in run.socks.iter().enumerate() {
        assert_eq!(s.fate, Fate::Parked, "slot {i} parked");
        assert_eq!(s.park_rule, Some(Rule::A), "slot {i} parked on rule (a)");
        let parked_after = s.parked_at.unwrap().saturating_duration_since(run.start);
        assert!(
            parked_after < Duration::from_secs(60),
            "slot {i} parked after {parked_after:?}: rule (a) is the second 808, not a timeout"
        );
        assert_eq!(
            s.oracle.hard_failures, 0,
            "slot {i} parked on rule (a), not on failures"
        );
    }
}

/// C: the mint endpoint fails outright for the whole hour. Every refresh
/// redial the supervisor asks for waits at least the 5 s token floor, so no
/// socket refreshes more often than the floor allows in any 300 s window;
/// parks only on (b) or (c); every socket parks in bounded time.
#[test]
fn s7c_mint_failing_outright_parks_only_on_rules_b_or_c_and_refreshes_are_floored() {
    let run = run_open(
        HOUR,
        |_, _, _, _| Verdict::Reject808,
        |_| Mint::Fail,
        no_sibling,
    );
    assert!(run.violations.is_empty(), "{:#?}", run.violations);
    assert_eq!(run.token.mints, 0);
    let window = Duration::from_millis(AUTH_REJECT_WINDOW_MS);
    let per_window =
        usize::try_from(AUTH_REJECT_WINDOW_MS / TOKEN_STALE_REDIAL_FLOOR_MS).unwrap() + 1;
    let bound = Duration::from_millis(AUTH_REJECT_EPISODE_MAX_MS) + 3 * max_delay(&run);
    for (i, s) in run.socks.iter().enumerate() {
        assert!(!s.refresh_delays.is_empty(), "slot {i} asked for a refresh");
        for d in &s.refresh_delays {
            assert!(
                *d >= TOKEN_STALE_REDIAL_FLOOR_MS,
                "slot {i}: a refresh redial after {d} ms, under the {TOKEN_STALE_REDIAL_FLOOR_MS} ms floor"
            );
        }
        for (k, t) in s.refreshes.iter().enumerate() {
            let in_window = s.refreshes[k..]
                .iter()
                .filter(|u| u.saturating_duration_since(*t) < window)
                .count();
            assert!(
                in_window <= per_window,
                "slot {i}: {in_window} refreshes inside one 300 s window"
            );
        }
        assert_eq!(s.fate, Fate::Parked, "slot {i} parked");
        assert!(
            matches!(s.park_rule, Some(Rule::B | Rule::C)),
            "slot {i} parked on {:?}, not (b) or (c)",
            s.park_rule
        );
        let after = s.parked_at.unwrap().saturating_duration_since(run.start);
        assert!(
            after <= bound,
            "slot {i} parked after {after:?} > {bound:?}"
        );
    }
    // Non-vacuity: the run reached BOTH branches. The sockets that made the
    // real attempts struck out on rule (b); the ones that only read the
    // reused failure back were deferred and parked on rule (c).
    let struck = run.rule_count(Rule::B);
    let deferred_only = run.rule_count(Rule::C);
    eprintln!("s7c: rule (b) {struck}, rule (c) {deferred_only}");
    assert!(struck >= 1, "rule (b) was reached");
    assert!(deferred_only >= 1, "rule (c) was reached");
}

/// D: every real attempt is deferred (mint cooldown / RESILIENCE-03). A
/// deferral never parks early: nothing parks before 300 s from its episode
/// start, and everything parks by 300 s plus three redial cycles.
#[test]
fn s7d_deferrals_never_park_a_socket_before_300_s_and_all_park_by_the_bound() {
    let run = run_open(
        HOUR,
        |_, _, _, _| Verdict::Reject808,
        |_| Mint::Defer,
        no_sibling,
    );
    assert!(run.violations.is_empty(), "{:#?}", run.violations);
    let episode = Duration::from_millis(AUTH_REJECT_EPISODE_MAX_MS);
    let bound = episode + 3 * max_delay(&run);
    for (i, s) in run.socks.iter().enumerate() {
        assert_eq!(s.fate, Fate::Parked, "slot {i} parked");
        assert_eq!(s.park_rule, Some(Rule::C), "slot {i} parked on rule (c)");
        let start = s.oracle.episode_start.unwrap();
        let after = s.parked_at.unwrap().saturating_duration_since(start);
        assert!(after >= episode, "slot {i} parked early, after {after:?}");
        assert!(
            after <= bound,
            "slot {i} parked late, after {after:?} > {bound:?}"
        );
        assert_eq!(s.oracle.hard_failures, 0, "a deferral is never a strike");
    }
}

/// E: the mint fails for the first 60 s, then works. Every socket that did
/// not reach 3 outright failures before 60 s comes back.
#[test]
fn s7e_mint_recovering_after_60_s_brings_back_every_socket_not_struck_out() {
    let mut first: Option<Instant> = None;
    let run = run_open(
        HOUR,
        |_, presented, _, _| {
            if presented == 0 {
                Verdict::Reject808
            } else {
                Verdict::Accept
            }
        },
        move |now| {
            let t0 = *first.get_or_insert(now);
            if now.saturating_duration_since(t0) < Duration::from_secs(60) {
                Mint::Fail
            } else {
                Mint::Ok
            }
        },
        no_sibling,
    );
    assert!(run.violations.is_empty(), "{:#?}", run.violations);
    // Model-level: the model never mints once the generation moved.
    assert_eq!(
        run.token.mints, 1,
        "one shared mint once the endpoint heals"
    );
    for (i, s) in run.socks.iter().enumerate() {
        if s.oracle.hard_failures < u32::from(AUTH_REJECT_MAX_HARD_FAILURES) {
            assert_eq!(s.fate, Fate::Live, "slot {i} came back");
        } else {
            assert_eq!(s.park_rule, Some(Rule::B), "slot {i} struck out on (b)");
        }
    }
    let live = run.socks.iter().filter(|s| s.fate == Fate::Live).count();
    assert!(live >= 1, "the pool recovers");
}

/// Small deterministic generator (no new dependency, seeds are replayable).
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// F: a seeded random mix over 300 seeds. The oracle holds at every park,
/// nothing dials after a park, rule (d) holds on the record, every socket
/// ends live or parked, episodes end and restart, and parks on rules (a),
/// (b), (c) and (d) are each reached (rule (d) is also pinned by s7h).
#[test]
fn s7f_random_vendor_and_mint_mixes_never_park_outside_the_documented_rules() {
    let mut parks = 0usize;
    let mut lives = 0usize;
    let mut resets = 0u32;
    let mut tally = [0usize; 4];
    for seed in 0..300u64 {
        let mut rng = Lcg(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5);
        let mut mint_rng = Lcg(seed ^ 0x5DEE_CE66);
        let mut sib_rng = Lcg(seed.wrapping_add(0xA5A5_5A5A));
        let accept_pct = rng.below(70);
        let ok_pct = mint_rng.below(100);
        let sibling_pct = sib_rng.below(60);
        let mut sibling_left = 6u32;
        let run = run_open(
            HOUR,
            move |_, _, _, dial_no| {
                // Bounded so every run terminates: past 40 dials a slot is
                // refused, which the rules must still park.
                if dial_no > 40 {
                    return Verdict::Reject808;
                }
                let roll = rng.below(100);
                if roll < accept_pct / 4 {
                    Verdict::Accept
                } else if roll < accept_pct {
                    // Some closes come after a healthy session, some before.
                    let stream = rng.below(2) == 0;
                    let after = Duration::from_millis(if stream {
                        rng.below(2 * MIN_HEALTHY_SESSION_MS)
                    } else {
                        rng.below(25_000)
                    });
                    let code = match rng.below(5) {
                        0 | 1 => AUTH_808,
                        2 => Some(DisconnectCode::AccessTokenExpired),
                        3 => Some(DisconnectCode::AccessTokenInvalid),
                        _ => None,
                    };
                    Verdict::AcceptThen {
                        after,
                        code,
                        stream,
                    }
                } else {
                    Verdict::Reject808
                }
            },
            move |_| {
                let roll = mint_rng.below(100);
                if roll < ok_pct {
                    Mint::Ok
                } else if roll.is_multiple_of(2) {
                    Mint::Fail
                } else {
                    Mint::Defer
                }
            },
            move |_| {
                // Capped: an unbounded external minter is not production.
                if sibling_left > 0 && sib_rng.below(100) < sibling_pct {
                    sibling_left -= 1;
                    true
                } else {
                    false
                }
            },
        );
        assert!(
            run.violations.is_empty(),
            "seed {seed}: {:#?}",
            run.violations
        );
        for (i, s) in run.socks.iter().enumerate() {
            assert_ne!(
                s.fate,
                Fate::Waiting,
                "seed {seed}: slot {i} neither live nor parked after an hour \
                 (redialling forever)"
            );
        }
        parks += run.socks.iter().filter(|s| s.fate == Fate::Parked).count();
        lives += run.socks.iter().filter(|s| s.fate == Fate::Live).count();
        resets += run.episode_resets();
        for (k, rule) in [Rule::A, Rule::B, Rule::C, Rule::D].into_iter().enumerate() {
            tally[k] += run.rule_count(rule);
        }
    }
    eprintln!("s7f: parks {parks}, live {lives}, episode resets {resets}, rules a/b/c/d {tally:?}");
    // Non-vacuity: the mix reached both ends, ended episodes, and each rule.
    assert!(parks > 100, "only {parks} parks across 300 seeds");
    assert!(lives > 100, "only {lives} live sockets across 300 seeds");
    assert!(resets > 0, "no episode ever ended and restarted");
    assert!(tally[0] > 0, "rule (a) never reached");
    assert!(tally[1] > 0, "rule (b) never reached");
    assert!(tally[2] > 0, "rule (c) never reached");
    assert!(tally[3] > 0, "rule (d) never reached");
}

// ---------------------------------------------------------------------------
// Single-supervisor deterministic scenarios.
// ---------------------------------------------------------------------------

fn one_slot() -> ConnectionSlot {
    all_slots().remove(0)
}

/// Dial and get an 808 on the handshake at `at`.
fn dial_808(s: &mut ConnectionSupervisor, at: Instant) -> SupervisorAction {
    assert_eq!(s.on_event(ConnEvent::BeginDial, at), SupervisorAction::Dial);
    let _ = s.on_event(ConnEvent::DialSucceeded, at);
    s.on_event(ConnEvent::Disconnected { code: AUTH_808 }, at)
}

/// Dial, get accepted, stream from `at`, and close with `code` at `close_at`.
fn dial_stream_close(
    s: &mut ConnectionSupervisor,
    at: Instant,
    close_at: Instant,
    code: Option<DisconnectCode>,
) -> SupervisorAction {
    assert_eq!(s.on_event(ConnEvent::BeginDial, at), SupervisorAction::Dial);
    let _ = s.on_event(ConnEvent::DialSucceeded, at);
    let _ = s.on_event(ConnEvent::SubscribeAcked, at);
    let _ = s.on_event(ConnEvent::FrameReceived, at);
    let _ = s.on_event(ConnEvent::FrameReceived, close_at);
    s.on_event(ConnEvent::Disconnected { code }, close_at)
}

fn refresh_delay(action: SupervisorAction) -> u64 {
    match action {
        SupervisorAction::RefreshTokenThenDial { delay_ms } => delay_ms,
        other => panic!("expected RefreshTokenThenDial, got {other:?}"),
    }
}

fn report(presented: u64, outcome: TokenRefreshOutcome) -> TokenRefreshReport {
    TokenRefreshReport { presented, outcome }
}

/// G: a healthy session ends the episode. Two strikes, then a fresh token
/// that streams for longer than `MIN_HEALTHY_SESSION_MS`, then an 808: the
/// new episode verifies nothing (no rule-(a) park on the same generation) and
/// takes three new outright failures before it parks. Under the minimum, the
/// old episode carries on and the same generation parks on rule (a). A bare
/// reset after frames ends the episode too.
#[test]
fn s7g_healthy_session_then_808_starts_a_new_episode_and_never_parks_on_old_strikes() {
    let healthy = Duration::from_millis(MIN_HEALTHY_SESSION_MS + 1_000);
    let short = Duration::from_millis(MIN_HEALTHY_SESSION_MS - 1_000);
    for (label, stream_for, close) in [
        ("808 after a healthy session", healthy, AUTH_808),
        ("bare reset after frames", short, None),
        ("808 under the healthy minimum", short, AUTH_808),
    ] {
        let t0 = Instant::now();
        let mut s = ConnectionSupervisor::new(one_slot(), t0);
        let mut t = t0;
        // Old episode: two outright failures, then a fresh generation 1.
        refresh_delay(dial_808(&mut s, t));
        for _ in 0..2 {
            t += Duration::from_secs(10);
            assert_eq!(
                s.note_token_refresh(report(0, TokenRefreshOutcome::Failed), t),
                None
            );
            refresh_delay(dial_808(&mut s, t));
        }
        t += Duration::from_secs(10);
        assert_eq!(
            s.note_token_refresh(report(0, TokenRefreshOutcome::Fresh { current: 1 }), t),
            None,
            "{label}: the fresh token is not a park"
        );
        // Generation 1 is accepted and streams, then closes.
        let close_at = t + stream_for;
        let action = dial_stream_close(&mut s, t, close_at, close);
        t = close_at;
        let ended = close.is_none() || stream_for >= Duration::from_millis(MIN_HEALTHY_SESSION_MS);
        if close.is_none() {
            assert!(
                matches!(action, SupervisorAction::SleepThenDial { .. }),
                "{label}: a reset redials, got {action:?}"
            );
            // The next dial is rejected with 808: a NEW episode.
            t += Duration::from_secs(5);
            refresh_delay(dial_808(&mut s, t));
        } else {
            refresh_delay(action);
        }
        assert_eq!(
            s.token_refresh_request().skip_if_presented,
            if ended { None } else { Some(1) },
            "{label}: the verified generation"
        );
        t += Duration::from_secs(10);
        let first = s.note_token_refresh(report(1, TokenRefreshOutcome::Failed), t);
        if !ended {
            assert!(
                matches!(first, Some(SupervisorAction::Park { .. })),
                "{label}: the old episode verified generation 1, so its 808 parks on (a)"
            );
            continue;
        }
        assert_eq!(first, None, "{label}: the new episode refreshes");
        // Two more outright failures are allowed, the third parks.
        for strike in 2..=u32::from(AUTH_REJECT_MAX_HARD_FAILURES) {
            let a = dial_808(&mut s, t);
            assert!(
                matches!(a, SupervisorAction::RefreshTokenThenDial { .. }),
                "{label}: strike {strike} still refreshes, got {a:?}"
            );
            t += Duration::from_secs(10);
            assert_eq!(
                s.note_token_refresh(report(1, TokenRefreshOutcome::Failed), t),
                None
            );
        }
        let last = dial_808(&mut s, t);
        assert!(
            matches!(last, SupervisorAction::Park { .. }),
            "{label}: three new strikes park on (b), got {last:?}"
        );
    }
}

/// H: rule (d). A sibling mints between this socket's refresh and its dial
/// every cycle, so each dial presents a generation the episode never
/// verified (rule (a) never fires), every refresh is fresh, and the account
/// rejects each one. The 4th 808 still refreshes; the 5th parks on the
/// refresh ceiling.
#[test]
fn s7h_sibling_driven_fresh_tokens_park_on_the_fifth_808_rule_d() {
    let t0 = Instant::now();
    let mut s = ConnectionSupervisor::new(one_slot(), t0);
    let mut token = SharedToken::new();
    let mut t = t0;
    let mut presented = token.generation;
    for n in 1..=AUTH_REJECT_MAX_REFRESHES_PER_WINDOW + 1 {
        assert_eq!(s.on_event(ConnEvent::BeginDial, t), SupervisorAction::Dial);
        let _ = s.on_event(ConnEvent::DialSucceeded, t);
        let action = s.on_event(ConnEvent::Disconnected { code: AUTH_808 }, t);
        if n <= AUTH_REJECT_MAX_REFRESHES_PER_WINDOW {
            let delay = refresh_delay(action);
            t += Duration::from_millis(delay);
            let r = token.refresh(s.token_refresh_request(), presented, t, Mint::Ok);
            assert!(matches!(r.outcome, TokenRefreshOutcome::Fresh { .. }));
            assert_eq!(s.note_token_refresh(r, t), None, "808 #{n} refreshes");
            // A sibling mints before this socket's dial lands.
            token.sibling_mint();
            presented = token.generation;
        } else {
            assert!(
                matches!(
                    action,
                    SupervisorAction::Park {
                        reason: ParkReason::FatalDisconnect
                    }
                ),
                "808 #{n} parks on the refresh ceiling, got {action:?}"
            );
        }
    }
    assert!(
        t.saturating_duration_since(t0) < Duration::from_millis(AUTH_REJECT_WINDOW_MS),
        "all four fresh tokens fell inside one window"
    );
    assert_eq!(s.phase(), ConnPhase::Parked);
}

/// Rule (c) at its exact boundary, and the grace refresh.
#[test]
fn s7_rule_c_exact_300s_boundary_and_grace_refresh() {
    let bound = Duration::from_millis(AUTH_REJECT_EPISODE_MAX_MS);
    // A refreshed cycle: 1 ms under the bound refreshes, at the bound parks.
    let t0 = Instant::now();
    let mut s = ConnectionSupervisor::new(one_slot(), t0);
    refresh_delay(dial_808(&mut s, t0));
    let t = t0 + Duration::from_secs(5);
    assert_eq!(
        s.note_token_refresh(report(0, TokenRefreshOutcome::Deferred), t),
        None
    );
    let under = t0 + bound - Duration::from_millis(1);
    refresh_delay(dial_808(&mut s, under));
    assert_eq!(
        s.note_token_refresh(report(0, TokenRefreshOutcome::Deferred), under),
        None
    );
    let at = dial_808(&mut s, t0 + bound);
    assert!(
        matches!(at, SupervisorAction::Park { .. }),
        "an 808 at exactly 300 s after a refreshed cycle parks, got {at:?}"
    );

    // A cycle with no refresh (a bare reset in between): past the bound the
    // first 808 takes the grace refresh, the next 808 parks, and a reset in
    // between does not buy another one.
    let t0 = Instant::now();
    let mut s = ConnectionSupervisor::new(one_slot(), t0);
    refresh_delay(dial_808(&mut s, t0));
    let t = t0 + Duration::from_secs(5);
    assert_eq!(
        s.note_token_refresh(report(0, TokenRefreshOutcome::Deferred), t),
        None
    );
    // Dial, accepted with no frame, bare reset at +100 s: the cycle is not a
    // refreshed one any more.
    let t = t0 + Duration::from_secs(100);
    assert_eq!(s.on_event(ConnEvent::BeginDial, t), SupervisorAction::Dial);
    let _ = s.on_event(ConnEvent::DialSucceeded, t);
    let _ = s.on_event(ConnEvent::SubscribeAcked, t);
    let reset = s.on_event(ConnEvent::Disconnected { code: None }, t);
    assert!(matches!(reset, SupervisorAction::SleepThenDial { .. }));
    let past = t0 + bound + Duration::from_millis(1);
    let grace = dial_808(&mut s, past);
    assert!(
        matches!(grace, SupervisorAction::RefreshTokenThenDial { .. }),
        "past the bound with no refresh this cycle: one grace refresh, got {grace:?}"
    );
    let t = past + Duration::from_secs(5);
    assert_eq!(
        s.note_token_refresh(report(0, TokenRefreshOutcome::Deferred), t),
        None
    );
    // A reset again, then an 808: the grace is spent, so it parks.
    let t = t + Duration::from_secs(1);
    assert_eq!(s.on_event(ConnEvent::BeginDial, t), SupervisorAction::Dial);
    let _ = s.on_event(ConnEvent::DialSucceeded, t);
    let _ = s.on_event(ConnEvent::SubscribeAcked, t);
    let _ = s.on_event(ConnEvent::Disconnected { code: None }, t);
    let parked = dial_808(&mut s, t + Duration::from_secs(1));
    assert!(
        matches!(parked, SupervisorAction::Park { .. }),
        "the grace refresh is spent: the next 808 past the bound parks, got {parked:?}"
    );
}

/// I: an 807 or 809 refresh inside an open 808 episode is ignored by the
/// episode: it never verifies the generation it produced (so the next 808
/// presenting it does not park on rule (a)) and an outright failure is never
/// a strike.
#[test]
fn s7i_807_refresh_inside_808_episode_neither_verifies_nor_strikes() {
    for stale in [
        DisconnectCode::AccessTokenExpired,
        DisconnectCode::AccessTokenInvalid,
    ] {
        let t0 = Instant::now();
        let mut s = ConnectionSupervisor::new(one_slot(), t0);
        let mut t = t0;
        refresh_delay(dial_808(&mut s, t));
        t += Duration::from_secs(5);
        // One real strike in the 808 episode, then a fresh generation 1.
        assert_eq!(
            s.note_token_refresh(report(0, TokenRefreshOutcome::Failed), t),
            None
        );
        refresh_delay(dial_808(&mut s, t));
        t += Duration::from_secs(5);
        assert_eq!(
            s.note_token_refresh(report(0, TokenRefreshOutcome::Fresh { current: 1 }), t),
            None
        );
        // Three 807/809 cycles, each refresh failing outright.
        for _ in 0..3 {
            assert_eq!(s.on_event(ConnEvent::BeginDial, t), SupervisorAction::Dial);
            let _ = s.on_event(ConnEvent::DialSucceeded, t);
            let _ = s.on_event(ConnEvent::SubscribeAcked, t);
            let a = s.on_event(ConnEvent::Disconnected { code: Some(stale) }, t);
            t += Duration::from_millis(refresh_delay(a));
            assert_eq!(
                s.token_refresh_request().skip_if_presented,
                None,
                "{stale:?}: an 807/809 refresh never skips"
            );
            assert_eq!(
                s.note_token_refresh(report(1, TokenRefreshOutcome::Failed), t),
                None,
                "{stale:?}: the episode ignores an 807/809 refresh"
            );
        }
        // An 807/809 refresh that produced generation 2.
        assert_eq!(s.on_event(ConnEvent::BeginDial, t), SupervisorAction::Dial);
        let _ = s.on_event(ConnEvent::DialSucceeded, t);
        let _ = s.on_event(ConnEvent::SubscribeAcked, t);
        let a = s.on_event(ConnEvent::Disconnected { code: Some(stale) }, t);
        t += Duration::from_millis(refresh_delay(a));
        assert_eq!(
            s.note_token_refresh(report(1, TokenRefreshOutcome::Fresh { current: 2 }), t),
            None
        );
        // An 808 presenting 2: the episode still holds 1 as verified and one
        // strike, so it refreshes; presenting 2 is not a rule-(a) park.
        let a = dial_808(&mut s, t);
        assert!(
            matches!(a, SupervisorAction::RefreshTokenThenDial { .. }),
            "{stale:?}: one strike plus ignored 807 failures, got {a:?}"
        );
        assert_eq!(s.token_refresh_request().skip_if_presented, Some(1));
        t += Duration::from_secs(5);
        assert_eq!(
            s.note_token_refresh(report(2, TokenRefreshOutcome::Failed), t),
            None,
            "{stale:?}: generation 2 was never verified, so no rule-(a) park"
        );
        // Strike 2 of 3: one more 808 still refreshes.
        let a = dial_808(&mut s, t);
        assert!(
            matches!(a, SupervisorAction::RefreshTokenThenDial { .. }),
            "{stale:?}: two strikes, got {a:?}"
        );
    }
}

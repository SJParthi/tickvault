//! Audit PR L1 (2026-10-26): a Dhan disconnect code that arrives as a write
//! fails, or during the close that follows, decides what the socket does next.
//!
//! Before the fix, `await_write` answered a failed write ticket first (biased
//! select) and returned no socket decision, the caller reported
//! `SubscribeFailed`, and `close` erased the owed code. An owed 805 therefore
//! redialled instead of parking (and never set the 805 breaker). These tests
//! live in their own file so `pool_supervisor.rs` stays under its size pin;
//! they are a child of `tests` and use its fakes.

use super::*;

/// A failed subscribe write while a stacked 805 is owed parks the socket for
/// 805 on the first dial; it never redials.
#[tokio::test(start_paused = true)]
async fn test_regression_failed_write_with_an_owed_805_parks() {
    let st = std::sync::Arc::new(Mutex::new(FakeState {
        subscribe_results: VecDeque::from(vec![false]),
        owed_close: Some(DisconnectCode::ExceededActiveConnections),
        ..FakeState::default()
    }));
    let sink = std::sync::Arc::new(RecordingSink::default());
    let guard =
        SubscribeGuard::try_new(DhanEndpointType::MainFeed, instruments(10)).expect("inside cap");

    let exit = run_connection(
        fake(&st),
        sup(DhanEndpointType::MainFeed, 0, t0()),
        guard,
        sink,
        no_refresh(),
    )
    .await;

    assert_eq!(exit, ConnectionExit::Parked(ParkReason::PoolOverflow));
    let s = st.lock().expect("fake state");
    assert_eq!(s.connects, 1, "an owed 805 must never be redialled");
    assert!(s.owed_close.is_none(), "the owed code was taken");
}

/// A failed subscribe write with nothing owed, where the close then reads an
/// 805 during its handshake, also parks: the code the close found replaces
/// the subscribe-failed redial.
#[tokio::test(start_paused = true)]
async fn test_regression_an_805_read_during_the_close_parks() {
    let st = std::sync::Arc::new(Mutex::new(FakeState {
        subscribe_results: VecDeque::from(vec![false]),
        close_code: Some(DisconnectCode::ExceededActiveConnections),
        ..FakeState::default()
    }));
    let sink = std::sync::Arc::new(RecordingSink::default());
    let guard =
        SubscribeGuard::try_new(DhanEndpointType::MainFeed, instruments(10)).expect("inside cap");

    let exit = run_connection(
        fake(&st),
        sup(DhanEndpointType::MainFeed, 0, t0()),
        guard,
        sink,
        no_refresh(),
    )
    .await;

    assert_eq!(exit, ConnectionExit::Parked(ParkReason::PoolOverflow));
    assert_eq!(st.lock().expect("fake state").connects, 1);
}

/// A failed write with nothing owed and nothing read at the close keeps the
/// old behaviour: the subscribe-failed redial.
#[tokio::test(start_paused = true)]
async fn a_failed_write_with_no_code_still_redials() {
    let st = std::sync::Arc::new(Mutex::new(FakeState {
        subscribe_results: VecDeque::from(vec![false]),
        ..FakeState::default()
    }));
    let sink = std::sync::Arc::new(RecordingSink::default());
    let guard =
        SubscribeGuard::try_new(DhanEndpointType::MainFeed, instruments(10)).expect("inside cap");

    let exit = run_connection(
        fake(&st),
        sup(DhanEndpointType::MainFeed, 0, t0()),
        guard,
        sink,
        no_refresh(),
    )
    .await;

    // The script's terminator (810) ends the second connection.
    assert_eq!(exit, ConnectionExit::Parked(ParkReason::FatalDisconnect));
    assert_eq!(st.lock().expect("fake state").connects, 2);
}

/// The late code is fed in only when it changes the decision.
#[test]
fn test_late_close_decision_feeds_only_codes_that_change_the_redial() {
    // No code, or a transient one: the redial already chosen stands.
    let mut s = sup(DhanEndpointType::MainFeed, 0, t0());
    assert_eq!(late_close_decision(&mut s, None, false), None);
    assert_eq!(
        late_close_decision(&mut s, Some(DisconnectCode::InternalServerError), false),
        None
    );
    // 807 in the refresh arm: already refreshing, not fed again.
    assert_eq!(
        late_close_decision(&mut s, Some(DisconnectCode::AccessTokenExpired), true),
        None
    );
    // 807 in the plain redial arm: the supervisor asks for a refresh.
    let decided = late_close_decision(&mut s, Some(DisconnectCode::AccessTokenExpired), false);
    assert!(
        matches!(decided, Some(SupervisorAction::RefreshTokenThenDial { .. })),
        "807 must refresh, got {decided:?}"
    );
    // 806 parks whichever arm found it.
    let mut s = sup(DhanEndpointType::MainFeed, 0, t0());
    assert_eq!(
        late_close_decision(
            &mut s,
            Some(DisconnectCode::DataApiSubscriptionRequired),
            true
        ),
        Some(SupervisorAction::Park {
            reason: ParkReason::FatalDisconnect
        })
    );
}

/// `owed_close_decision` reads nothing and decides nothing when no code is
/// owed, and takes the code when one is.
#[test]
fn test_owed_close_decision_takes_the_owed_code_once() {
    let st = std::sync::Arc::new(Mutex::new(FakeState {
        owed_close: Some(DisconnectCode::DataApiSubscriptionRequired),
        ..FakeState::default()
    }));
    let mut sock = fake(&st);
    let mut s = sup(DhanEndpointType::MainFeed, 0, t0());
    assert_eq!(
        owed_close_decision(&mut sock, &mut s),
        Some(SupervisorAction::Park {
            reason: ParkReason::FatalDisconnect
        })
    );
    let mut s = sup(DhanEndpointType::MainFeed, 0, t0());
    assert_eq!(owed_close_decision(&mut sock, &mut s), None, "taken once");
}

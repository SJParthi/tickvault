//! Guard — depth-200 gets a client keepalive, and the watchdog still measures
//! what it says it measures.
//!
//! # The defect
//!
//! `IdleWatchdog`'s own header states its purpose: *"this timer is not merely
//! 'is the server quiet?' — it is also, and mostly, **'have WE stopped
//! draining?'**"*. That works because Dhan pings every ~10s, our library
//! auto-pongs **only while the read loop is polling**, and arriving traffic
//! therefore proves we are draining.
//!
//! Measured on prod 2026-08-26, `tv_dhan_ws_control_frames_total{kind="ping"}`:
//! `main_feed` 3,460 · `depth_20` 12,110 · **`depth_200` — series absent
//! entirely**. The counter is created lazily on first ping, so an absent series
//! means depth-200 received no control frame at all in a whole session, despite
//! `docs/dhan-ref/full-market-depth.md:107` claiming it pings every 10 seconds.
//!
//! With nothing to pong, the watchdog on that endpoint silently degrades from
//! measuring OUR health to measuring **whether an illiquid option happens to be
//! trading**. Same session: `main_feed` and `order_update` took **0**
//! disconnects; `depth_200` took **265**, every one self-inflicted at
//! `idle_secs: 27`. Three sockets cycled 19 times each then stopped dead at
//! 09:14:45 — nothing was fixed at 09:15, the market opened.
//!
//! # The invariant that must not regress
//!
//! The watchdog reset stays on the **received Pong**, never on the ping SEND.
//! Resetting on send would defeat the watchdog outright: we would always look
//! active because we always ping, and a reader that had stopped draining would
//! never be caught. This file pins that distinction, because it is the
//! difference between a fix and a silent disabling of the safety net.

use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_core::websocket::pool_supervisor::{
    CLIENT_KEEPALIVE_PING_INTERVAL, IDLE_POLL_INTERVAL,
};

use std::path::PathBuf;

fn source(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn only_depth200_needs_a_client_keepalive() {
    assert!(
        DhanEndpointType::Depth200.needs_client_keepalive_ping(),
        "depth-200 receives ZERO server pings (measured 2026-08-26), so without \
         a client-originated ping its idle watchdog measures option liquidity \
         rather than our own draining"
    );

    for endpoint in [
        DhanEndpointType::MainFeed,
        DhanEndpointType::Depth20,
        DhanEndpointType::OrderUpdate,
    ] {
        assert!(
            !endpoint.needs_client_keepalive_ping(),
            "{endpoint:?} already receives server pings, and the 2026-08-19 fix \
             already turns those into watchdog resets (main_feed went from 2,150 \
             disconnects/day to 0). Adding redundant control traffic to a working \
             path buys nothing."
        );
    }
}

#[test]
fn every_endpoint_answers_the_keepalive_question() {
    // Non-vacuity: if a fifth endpoint is ever added, it must make a deliberate
    // choice rather than inherit one. `ALL` is the enum's own list, so this
    // fails to compile-or-pass if a variant is added without being considered.
    assert_eq!(
        DhanEndpointType::ALL.len(),
        4,
        "the scope lock permits exactly four endpoint types; a fifth needs its \
         own keepalive decision AND a dated operator quote"
    );
    let needing = DhanEndpointType::ALL
        .iter()
        .filter(|e| e.needs_client_keepalive_ping())
        .count();
    assert_eq!(
        needing, 1,
        "exactly one endpoint (depth-200) should need a client keepalive today"
    );
}

#[test]
fn two_pings_fit_inside_the_idle_window() {
    // The const assertion in pool_supervisor.rs enforces this at compile time;
    // this restates it as a runtime test so the REASON is discoverable from the
    // test name when someone tunes either number.
    use tickvault_core::websocket::idle_watchdog::IDLE_RECONNECT_TIMEOUT_SECS;

    let pings_per_window =
        IDLE_RECONNECT_TIMEOUT_SECS as f64 / CLIENT_KEEPALIVE_PING_INTERVAL.as_secs() as f64;
    assert!(
        pings_per_window >= 2.0,
        "only {pings_per_window:.1} keepalive pings fit inside the {IDLE_RECONNECT_TIMEOUT_SECS}s \
         idle window — a single lost ping or pong could then expire a healthy socket"
    );
}

#[test]
fn keepalive_is_never_faster_than_the_ticker_that_drives_it() {
    assert!(
        CLIENT_KEEPALIVE_PING_INTERVAL >= IDLE_POLL_INTERVAL,
        "the keepalive rides the idle ticker, so an interval below the tick \
         period would silently become the tick period — a constant that lies"
    );
}

#[test]
fn keepalive_stays_inside_dhans_documented_client_silence_close() {
    use tickvault_core::websocket::idle_watchdog::DHAN_SERVER_CLOSE_AFTER_SILENCE_SECS;

    assert!(
        CLIENT_KEEPALIVE_PING_INTERVAL.as_secs() * 2 < DHAN_SERVER_CLOSE_AFTER_SILENCE_SECS,
        "two keepalive intervals must fit inside Dhan's documented {DHAN_SERVER_CLOSE_AFTER_SILENCE_SECS}s \
         client-silence close, so one dropped ping cannot let the server close us"
    );
}

#[test]
fn the_watchdog_reset_stays_on_the_received_pong_not_on_the_send() {
    // THE load-bearing invariant of this change.
    //
    // `record_activity` must be reachable from the KeepAliveReceived arm (the
    // pong we get back) and must NOT appear in the ticker arm that sends the
    // ping. If a future edit resets on send, the watchdog stops being able to
    // detect a reader that has stopped draining — the exact failure it exists
    // for — while every metric still looks healthy.
    let src = source("websocket/pool_supervisor.rs");

    let recv_arm = src
        .find("ConnEvent::KeepAliveReceived =>")
        .expect("the KeepAliveReceived arm must exist");
    let recv_body = &src[recv_arm..(recv_arm + 300).min(src.len())];
    assert!(
        recv_body.contains("record_activity"),
        "the received-pong arm must reset the idle watchdog — that is the \
         entire mechanism this fix relies on"
    );

    // The send site must not reset anything.
    let send_at = src
        .find("socket.send_ping()")
        .expect("the client keepalive send site must exist");
    let window_start = send_at.saturating_sub(1_200);
    let send_window = &src[window_start..(send_at + 200).min(src.len())];
    assert!(
        !send_window.contains("record_activity"),
        "the keepalive SEND must never reset the idle watchdog. Resetting on \
         send defeats the watchdog: we would always look active because we \
         always ping, and a reader that stopped draining would never be caught."
    );
}

#[test]
fn the_send_is_gated_on_the_endpoint_predicate() {
    // A keepalive sent on every endpoint would add control traffic to three
    // paths that already work, and would make the measurement that justified
    // this change unreproducible.
    let src = source("websocket/pool_supervisor.rs");
    let send_at = src
        .find("socket.send_ping()")
        .expect("the client keepalive send site must exist");
    let window = &src[send_at.saturating_sub(400)..send_at];
    assert!(
        window.contains("needs_client_keepalive_ping()"),
        "the keepalive send must be gated on the endpoint predicate"
    );
    assert!(
        window.contains("CLIENT_KEEPALIVE_PING_INTERVAL"),
        "the keepalive send must be paced by the named interval, not by the \
         1-second idle tick"
    );
}

#[test]
fn a_failed_ping_does_not_tear_down_the_socket() {
    // Fail-safe direction. If Dhan does not answer pings on this endpoint the
    // watchdog simply keeps governing the socket exactly as it does today —
    // this change can improve that path, never worsen it.
    let src = source("websocket/pool_supervisor.rs");
    let send_at = src
        .find("socket.send_ping()")
        .expect("the client keepalive send site must exist");
    let line_start = src[..send_at].rfind('\n').map_or(0, |i| i + 1);
    let line = &src[line_start..(send_at + 40).min(src.len())];
    assert!(
        line.contains("let _ ="),
        "a failed keepalive must be ignored at the call site: escalating it \
         would turn 'Dhan does not answer pings here' into a disconnect, which \
         is worse than the behaviour being fixed"
    );
}

//! End to end: the PRODUCTION socket type (`DhanFeedSocketImpl`) and the
//! PRODUCTION connection loop (`run_connection`) against a fake Dhan server on
//! a loopback port. Audit PROOF-2, 2026-10-11.
//!
//! ## Why this file exists
//!
//! Before it, no test ran `DhanFeedSocketImpl::connect`, its subscribe writes
//! or its `recv` against a real socket. The `run_connection` unit tests drive
//! a fake transport; `chaos_ws_mock_server.rs` and
//! `chaos_ws_e2e_wal_durability.rs` drive raw `tokio_tungstenite` streams,
//! never the production type. So the URL the socket dials, the JSON it writes
//! and the way it turns a stacked disconnect packet into a supervisor decision
//! were each tested only in pieces.
//!
//! ## What one run proves
//!
//! The server accepts two connections and the client does exactly what
//! production does, through `WalRingSink` into a real `WsFrameSpill`:
//!
//! 1. **Dial.** The upgrade path is `/` and the query is exactly
//!    `version=2&token=<t>&clientId=<id>&authType=2`.
//! 2. **Subscribe.** Every instrument is covered; `SecurityId` is a JSON
//!    STRING; `ExchangeSegment` is the string name; the index sits in its own
//!    message at Quote (17) and the derivatives at Full (21);
//!    `InstrumentCount` equals the list length.
//! 3. **807.** Connection 1 sends 50 ticker frames, then one frame stacking
//!    ticker 51 and an 807 disconnect packet. The stacked ticker is handed up
//!    as data (audit PR21), the token is refreshed ONCE, and the client
//!    redials with the NEW token in its URL and re-sends the whole set.
//! 4. **805.** Connection 2 sends 9 ticker frames, then ticker 61 stacked with
//!    an 805. The client parks (`ParkReason::PoolOverflow`) and never dials a
//!    third time.
//! 5. **Capture.** The ring holds all 61 frames byte for byte, in order, and a
//!    WAL replay returns the same 61 frames with the same sequence numbers.
//!
//! ## Honest limits
//!
//! * Plain `ws://` on loopback: TLS, ALPN and the vendor's real server are not
//!   exercised. `connect_async_tls_with_config` picks plain mode for a `ws://`
//!   URL and ignores the connector.
//! * The 805 sets process-wide state (the rotation breaker and the overflow
//!   episodes), so the whole scenario is ONE test in its OWN binary, run in
//!   order: the 807 leg first, then the 805.
//! * Real time, not a paused clock: the 807 redial waits the 5 s token floor.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use tickvault_common::types::ExchangeSegment;
use tickvault_core::websocket::connection::{DhanFeedSocketImpl, DhanSocketParams};
use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_core::websocket::pool_supervisor::{
    CapturedFrame, ConnectionExit, ConnectionSupervisor, ParkReason, PoolSupervisor,
    RingByteBudget, SubscribeGuard, SubscribeInstrument, TokenRefreshOutcome, TokenRefreshReport,
    TokenRefreshRequest, WalRingSink, run_connection,
};
use tickvault_storage::ws_frame_spill::{WsFrameSpill, WsType, replay_all};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use zeroize::Zeroizing;

const CLIENT_ID: &str = "1000000001";
const TOKEN_PREFIX: &str = "test.jwt.token.";
/// Dhan response code of a disconnect packet; reason u16 LE at bytes 8..10.
const DISCONNECT_CODE: u8 = 50;
const DISCONNECT_LEN: u16 = 10;
const REASON_TOKEN_EXPIRED: u16 = 807;
const REASON_TOO_MANY_CONNECTIONS: u16 = 805;
/// Subscribe request codes: Full for derivatives, Quote for the index.
const REQUEST_CODE_FULL: u64 = 21;
const REQUEST_CODE_QUOTE: u64 = 17;
/// Ticker frames sent as their own frame on each connection, before the
/// stacked one.
const CONN1_SINGLE_TICKS: u32 = 50;
const CONN2_SINGLE_TICKS: u32 = 9;
/// Whole scenario bound: one 5 s token floor plus generous slack.
const SCENARIO_TIMEOUT: Duration = Duration::from_secs(60);

/// The three instruments the client holds: two derivatives and one index, so
/// the index-split path runs.
fn held_instruments() -> Vec<SubscribeInstrument> {
    vec![
        SubscribeInstrument {
            security_id: 52_432,
            segment: ExchangeSegment::NseFno,
        },
        SubscribeInstrument {
            security_id: 52_433,
            segment: ExchangeSegment::NseFno,
        },
        SubscribeInstrument {
            security_id: 13,
            segment: ExchangeSegment::IdxI,
        },
    ]
}

/// A 16-byte Dhan ticker packet (code 2, NSE_FNO) with `marker` as its
/// security id, so every frame is distinct.
fn ticker(marker: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 16];
    buf[0] = 2;
    buf[1..3].copy_from_slice(&16u16.to_le_bytes());
    buf[3] = 2;
    buf[4..8].copy_from_slice(&marker.to_le_bytes());
    buf[8..12].copy_from_slice(&100.25_f32.to_le_bytes());
    buf[12..16].copy_from_slice(&1_700_000_000u32.to_le_bytes());
    buf
}

/// One frame: ticker `marker` followed by a 10-byte disconnect packet.
fn ticker_then_disconnect(marker: u32, reason: u16) -> Vec<u8> {
    let mut buf = ticker(marker);
    let mut disconnect = vec![0u8; usize::from(DISCONNECT_LEN)];
    disconnect[0] = DISCONNECT_CODE;
    disconnect[1..3].copy_from_slice(&DISCONNECT_LEN.to_le_bytes());
    disconnect[8..10].copy_from_slice(&reason.to_le_bytes());
    buf.extend_from_slice(&disconnect);
    buf
}

/// What the fake server saw on one connection.
#[derive(Debug, Default)]
struct SeenConnection {
    path: String,
    query: String,
    subscribes: Vec<serde_json::Value>,
    /// Every text message the client sent AFTER the subscribe covered the
    /// held set (2026-10-11 review): an extra subscribe, a repeat or any
    /// other JSON would otherwise be drained and ignored.
    late_texts: Vec<String>,
}

/// Accepts one upgrade, records the request target, reads subscribe messages
/// until every held instrument is covered, sends `frames`, then keeps reading
/// until the client goes away (which also answers its close handshake).
async fn serve_one(listener: &TcpListener, frames: Vec<Vec<u8>>) -> SeenConnection {
    let (tcp, _peer) = listener.accept().await.expect("accept"); // APPROVED: test
    let target: Arc<Mutex<(String, String)>> = Arc::default();
    let record = Arc::clone(&target);
    // APPROVED: the Result type is fixed by tungstenite's handshake `Callback` trait.
    #[allow(clippy::result_large_err)]
    let callback = move |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
        let uri = req.uri();
        if let Ok(mut t) = record.lock() {
            *t = (
                uri.path().to_string(),
                uri.query().unwrap_or_default().to_string(),
            );
        }
        Ok(resp)
    };
    let mut ws = tokio_tungstenite::accept_hdr_async(tcp, callback)
        .await
        .expect("websocket upgrade"); // APPROVED: test
    let (path, query) = target.lock().expect("target").clone(); // APPROVED: test

    let mut seen = SeenConnection {
        path,
        query,
        subscribes: Vec::new(),
        late_texts: Vec::new(),
    };
    let want = held_instruments().len();
    let mut covered = 0usize;
    while covered < want {
        match ws.next().await {
            Some(Ok(Message::Text(text))) => {
                let value: serde_json::Value =
                    serde_json::from_str(text.as_str()).expect("subscribe is JSON"); // APPROVED: test
                covered += value["InstrumentList"]
                    .as_array()
                    .map(Vec::len)
                    .unwrap_or_default();
                seen.subscribes.push(value);
            }
            Some(Ok(_)) => {}
            other => panic!("socket ended before the subscribe was complete: {other:?}"),
        }
    }
    for frame in frames {
        ws.send(Message::Binary(frame.into()))
            .await
            .expect("send frame"); // APPROVED: test
    }
    // Hold the socket open; the client closes it after the disconnect packet.
    // Record any text it sends meanwhile; the test asserts there is none.
    while let Some(Ok(message)) = ws.next().await {
        if let Message::Text(text) = message {
            seen.late_texts.push(text.as_str().to_string());
        }
    }
    seen
}

fn temp_wal_dir() -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!("tv-fake-dhan-e2e-{}-{nanos}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create temp WAL dir"); // APPROVED: test
    p
}

/// Checks one connection's subscribe messages against the held set.
fn assert_subscribes(conn: usize, seen: &SeenConnection) {
    assert!(
        seen.late_texts.is_empty(),
        "conn {conn}: the client sent text after its subscribe covered the held set: {:?}",
        seen.late_texts
    );
    let mut derivatives = 0usize;
    let mut indices = 0usize;
    for message in &seen.subscribes {
        let list = message["InstrumentList"]
            .as_array()
            .expect("InstrumentList array"); // APPROVED: test
        assert_eq!(
            message["InstrumentCount"].as_u64(),
            Some(list.len() as u64),
            "conn {conn}: InstrumentCount must equal the list length: {message}"
        );
        let segments: Vec<&str> = list
            .iter()
            .map(|i| i["ExchangeSegment"].as_str().expect("segment is a string")) // APPROVED: test
            .collect();
        for instrument in list {
            assert!(
                instrument["SecurityId"].is_string(),
                "conn {conn}: SecurityId must be a JSON STRING: {instrument}"
            );
        }
        if segments.contains(&"IDX_I") {
            assert!(
                segments.iter().all(|s| *s == "IDX_I"),
                "conn {conn}: an index must sit in its own message: {message}"
            );
            assert_eq!(message["RequestCode"].as_u64(), Some(REQUEST_CODE_QUOTE));
            indices += list.len();
        } else {
            assert!(segments.iter().all(|s| *s == "NSE_FNO"), "{message}");
            assert_eq!(message["RequestCode"].as_u64(), Some(REQUEST_CODE_FULL));
            derivatives += list.len();
        }
    }
    assert_eq!(
        (derivatives, indices),
        (2, 1),
        "conn {conn}: every held instrument subscribed exactly once"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_socket_dials_subscribes_refreshes_on_807_and_parks_on_805() {
    // Boot step 1 in production (`main.rs`): the TLS connector is built on
    // every dial, even for a `ws://` URL. `Err` means already installed.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback"); // APPROVED: test
    let port = listener.local_addr().expect("local addr").port(); // APPROVED: test

    // The script for both connections, and the frames the client must capture.
    let mut expected: Vec<Vec<u8>> = Vec::new();
    let mut conn1 = Vec::new();
    for marker in 1..=CONN1_SINGLE_TICKS {
        conn1.push(ticker(marker));
    }
    conn1.push(ticker_then_disconnect(
        CONN1_SINGLE_TICKS + 1,
        REASON_TOKEN_EXPIRED,
    ));
    let first_conn2 = CONN1_SINGLE_TICKS + 2;
    let mut conn2 = Vec::new();
    for marker in first_conn2..first_conn2 + CONN2_SINGLE_TICKS {
        conn2.push(ticker(marker));
    }
    conn2.push(ticker_then_disconnect(
        first_conn2 + CONN2_SINGLE_TICKS,
        REASON_TOO_MANY_CONNECTIONS,
    ));
    expected.extend(conn1.iter().cloned());
    expected.extend(conn2.iter().cloned());

    let server = tokio::spawn(async move {
        let first = serve_one(&listener, conn1).await;
        let second = serve_one(&listener, conn2).await;
        // A third dial after an 805 would be a bug; give it a moment to show.
        let third = tokio::time::timeout(Duration::from_millis(500), listener.accept())
            .await
            .is_ok();
        (first, second, third)
    });

    // The token source reads the CURRENT generation on every dial, as the
    // production token handle does; the refresh closure moves it.
    let generation = Arc::new(AtomicU64::new(1));
    let dial_generation = Arc::clone(&generation);
    let token = move || {
        Some(Zeroizing::new(format!(
            "{TOKEN_PREFIX}{}",
            dial_generation.load(Ordering::SeqCst)
        )))
    };
    let refreshes = Arc::new(AtomicUsize::new(0));
    let refresh_count = Arc::clone(&refreshes);
    let refresh_generation = Arc::clone(&generation);
    let refresh = move |_request: TokenRefreshRequest| {
        refresh_count.fetch_add(1, Ordering::SeqCst);
        let presented = refresh_generation.fetch_add(1, Ordering::SeqCst);
        std::future::ready(TokenRefreshReport {
            presented,
            outcome: TokenRefreshOutcome::Fresh {
                current: presented + 1,
            },
        })
    };

    let params = DhanSocketParams::new(
        DhanEndpointType::MainFeed,
        format!("ws://127.0.0.1:{port}"),
        CLIENT_ID.to_string(),
    );
    let socket = DhanFeedSocketImpl::new(params, token);

    let now = Instant::now();
    let mut pool = PoolSupervisor::new();
    let slot = pool
        .admit(DhanEndpointType::MainFeed, now)
        .expect("a main-feed slot"); // APPROVED: test
    let supervisor = ConnectionSupervisor::new(slot, now);
    let guard = SubscribeGuard::try_new(DhanEndpointType::MainFeed, held_instruments())
        .expect("inside the cap"); // APPROVED: test

    let dir = temp_wal_dir();
    let spill = Arc::new(WsFrameSpill::new(&dir).expect("WsFrameSpill::new")); // APPROVED: test
    let (ring_tx, mut ring_rx) =
        tickvault_core::websocket::frame_ring::frame_ring::<CapturedFrame>(1024);
    let budget = Arc::new(RingByteBudget::with_slot_cap(1 << 20, 1024));
    let sink = Arc::new(WalRingSink::new(
        Arc::clone(&spill),
        ring_tx,
        Arc::clone(&budget),
        WsType::LiveFeed,
        DhanEndpointType::MainFeed,
        slot.global_index,
    ));

    let exit = tokio::time::timeout(
        SCENARIO_TIMEOUT,
        run_connection(socket, supervisor, guard, sink, refresh),
    )
    .await
    .expect("the connection loop must end within the scenario bound"); // APPROVED: test
    assert_eq!(
        exit,
        ConnectionExit::Parked(ParkReason::PoolOverflow),
        "an 805 must park the socket"
    );
    assert_eq!(
        refreshes.load(Ordering::SeqCst),
        1,
        "the 807 must refresh the token exactly once before the redial"
    );

    let (first, second, third_dial) = server.await.expect("server task"); // APPROVED: test
    assert!(!third_dial, "an 805 must never be redialled");

    // (1) The URL, both dials. The second carries the refreshed token.
    for (conn, seen, gen_no) in [(1, &first, 1), (2, &second, 2)] {
        assert_eq!(seen.path, "/", "conn {conn}: upgrade path must be the root");
        assert_eq!(
            seen.query,
            format!("version=2&token={TOKEN_PREFIX}{gen_no}&clientId={CLIENT_ID}&authType=2"),
            "conn {conn}: exact query string"
        );
    }

    // (2) The subscribe JSON, both dials (the redial replays the whole set).
    assert_subscribes(1, &first);
    assert_subscribes(2, &second);

    // (5) The ring: every frame, byte for byte, in order, stacked ones
    // included.
    let mut ring: Vec<CapturedFrame> = Vec::new();
    while let Some(frame) = ring_rx.try_recv() {
        budget.release(frame.bytes.len());
        ring.push(frame);
    }
    assert_eq!(
        ring.len(),
        expected.len(),
        "every frame the server sent must reach the ring once"
    );
    for (i, (got, want)) in ring.iter().zip(&expected).enumerate() {
        assert_eq!(got.bytes.as_ref(), want.as_slice(), "ring frame {i}");
        assert_eq!(got.endpoint, DhanEndpointType::MainFeed);
    }

    // And the WAL: the same frames, the same sequence numbers.
    assert_eq!(spill.shutdown(Duration::from_secs(10)), 0, "no WAL drops");
    let replayed = replay_all(&dir).expect("replay_all"); // APPROVED: test
    assert_eq!(replayed.len(), expected.len(), "WAL frame count");
    for (i, (got, captured)) in replayed.iter().zip(&ring).enumerate() {
        assert_eq!(got.ws_type, WsType::LiveFeed, "WAL frame {i} type");
        assert_eq!(
            got.frame.as_slice(),
            captured.bytes.as_ref(),
            "WAL frame {i} bytes"
        );
        assert_eq!(got.frame_seq, captured.seq, "WAL frame {i} sequence");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

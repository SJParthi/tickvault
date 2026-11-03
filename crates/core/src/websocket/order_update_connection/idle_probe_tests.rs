#![cfg(test)]

//! Drive the production session over bounded in-memory WebSocket transports.
//! Per-session notification injects the idle signal without global clock gates.
//! The common watchdog tests own threshold/session gating; no broker I/O.

use std::io;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::sync::Notify;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::Role;

use super::*;

/// The fixture only transitions to blocked; it never needs an unblock wake.
struct GatedWrite {
    stream: DuplexStream,
    blocked: Arc<AtomicBool>,
    attempted: Arc<Notify>,
}

impl AsyncRead for GatedWrite {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buffer)
    }
}

impl AsyncWrite for GatedWrite {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.blocked.load(Ordering::SeqCst) {
            self.attempted.notify_one();
            Poll::Pending
        } else {
            Pin::new(&mut self.stream).poll_write(cx, buffer)
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

struct Fixture {
    peer: WebSocketStream<DuplexStream>,
    session: tokio::task::JoinHandle<Result<(), OrderUpdateConnectionError>>,
    orders: broadcast::Receiver<OrderUpdate>,
    active: Arc<metrics::atomics::AtomicU64>,
    blocked: Arc<AtomicBool>,
    attempted: Arc<Notify>,
    watchdog_notify: Arc<Notify>,
}

impl Fixture {
    async fn new() -> Self {
        let (client, peer) = tokio::io::duplex(4096);
        let blocked = Arc::new(AtomicBool::new(false));
        let attempted = Arc::new(Notify::new());
        let stream = WebSocketStream::from_raw_socket(
            GatedWrite {
                stream: client,
                blocked: Arc::clone(&blocked),
                attempted: Arc::clone(&attempted),
            },
            Role::Client,
            Some(order_update_websocket_config()),
        )
        .await;
        let mut peer = WebSocketStream::from_raw_socket(peer, Role::Server, None).await;
        let active = Arc::new(metrics::atomics::AtomicU64::new(0));
        let gauge = metrics::Gauge::from_arc(Arc::clone(&active));
        let (sender, orders) = broadcast::channel(4);
        let watchdog_notify = Arc::new(Notify::new());
        let session_notify = Arc::clone(&watchdog_notify);
        let session = tokio::spawn(async move {
            let mut stable = false;
            listen_on_connected_order_stream(
                stream,
                gauge,
                OrderUpdateSession {
                    client_id: "fixture-client",
                    access_token: "fixture-token",
                    order_sender: &sender,
                    wal_spill: None,
                    authenticated_signal: None,
                    authenticated_latch: None,
                    notifier: None,
                    failures_before_attempt: 0,
                    stability_reached: &mut stable,
                    test_watchdog_notify: Some(session_notify),
                },
            )
            .await
        });
        let login = time::timeout(Duration::from_secs(1), peer.next())
            .await
            .expect("login deadline")
            .expect("login frame")
            .expect("valid login frame");
        assert!(matches!(login, Message::Text(_)));
        tokio::task::yield_now().await;
        Self {
            peer,
            session,
            orders,
            active,
            blocked,
            attempted,
            watchdog_notify,
        }
    }

    fn signal_idle(&self) {
        self.watchdog_notify.notify_one();
    }

    async fn receive_probe(&mut self) -> Vec<u8> {
        self.signal_idle();
        let frame = time::timeout(Duration::from_secs(1), self.peer.next())
            .await
            .expect("probe send deadline")
            .expect("probe frame")
            .expect("valid probe");
        match frame {
            Message::Ping(payload) => {
                assert_eq!(payload.len(), 8, "bounded nonce Ping");
                payload.to_vec()
            }
            other => panic!("expected Ping, received {other:?}"),
        }
    }

    /// Write a small server frame without polling tungstenite: the queued
    /// automatic Pong must not accidentally satisfy a missing-Pong fixture.
    async fn raw_frame(&mut self, opcode: u8, payload: &[u8]) {
        assert!(payload.len() <= 125);
        let mut frame = vec![0x80 | opcode, payload.len() as u8];
        frame.extend_from_slice(payload);
        self.peer
            .get_mut()
            .write_all(&frame)
            .await
            .expect("fixture write");
    }

    async fn finish(&mut self) -> Result<(), OrderUpdateConnectionError> {
        let result = time::timeout(Duration::from_secs(1), &mut self.session)
            .await
            .expect("session termination deadline")
            .expect("session join");
        assert_eq!(f64::from_bits(self.active.load(Ordering::SeqCst)), 0.0);
        result
    }
}

#[tokio::test(start_paused = true)]
async fn test_order_idle_probe_matching_pong_preserves_session_and_accepts_later_probe() {
    let mut fixture = Fixture::new().await;
    let first = fixture.receive_probe().await;
    fixture
        .peer
        .flush()
        .await
        .expect("send automatic matching Pong");
    // The second idle notification cannot be consumed until the first probe
    // completes. Receiving its distinct Ping is an explicit completion barrier.
    let second = fixture.receive_probe().await;
    assert_ne!(first, second, "a later probe requires a new nonce");
    fixture.peer.flush().await.expect("answer second Ping");
    fixture
        .raw_frame(
            1,
            br#"{"Data":{"OrderNo":"after-pong","Status":"TRADED"},"Type":"order_alert"}"#,
        )
        .await;
    let update = time::timeout(Duration::from_secs(1), fixture.orders.recv())
        .await
        .expect("input completion barrier")
        .expect("order broadcast");
    assert_eq!(update.order_no, "after-pong");
    time::advance(idle_probe::RESPONSE_TIMEOUT + Duration::from_secs(1)).await;
    fixture.raw_frame(8, &[]).await;
    assert!(
        fixture.finish().await.is_ok(),
        "healthy quiet session survives prior probe deadlines"
    );
}

#[tokio::test(start_paused = true)]
async fn test_order_idle_probe_missing_or_wrong_pong_keeps_finite_deadline() {
    for wrong_pong in [false, true] {
        let mut fixture = Fixture::new().await;
        let mut nonce = fixture.receive_probe().await;
        if wrong_pong {
            nonce[0] ^= 1;
            fixture.raw_frame(10, &nonce).await;
            tokio::task::yield_now().await;
        }
        time::advance(idle_probe::RESPONSE_TIMEOUT).await;
        assert!(matches!(
            fixture.finish().await,
            Err(OrderUpdateConnectionError::LivenessProbeTimeout {
                phase: "awaiting Pong or inbound activity",
                timeout_secs: 10
            })
        ));
    }
}

#[tokio::test(start_paused = true)]
async fn test_order_idle_probe_processes_business_data_while_awaiting_pong() {
    let mut fixture = Fixture::new().await;
    fixture.receive_probe().await;
    fixture
        .raw_frame(
            1,
            br#"{"Data":{"OrderNo":"probe-order","Status":"TRADED"},"Type":"order_alert"}"#,
        )
        .await;
    let update = time::timeout(Duration::from_secs(1), fixture.orders.recv())
        .await
        .expect("order read stays live")
        .expect("order broadcast");
    assert_eq!(update.order_no, "probe-order");
    assert_eq!(update.status, "TRADED");
    time::advance(idle_probe::RESPONSE_TIMEOUT + Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert!(
        !fixture.session.is_finished(),
        "business input establishes liveness without Pong"
    );
    fixture.raw_frame(8, &[]).await;
    assert!(fixture.finish().await.is_ok());
}

async fn assert_blocked_probe_keeps_reading(server_ping: Option<&[u8]>) {
    let mut fixture = Fixture::new().await;
    fixture.blocked.store(true, Ordering::SeqCst);
    fixture.signal_idle();
    time::timeout(Duration::from_secs(1), fixture.attempted.notified())
        .await
        .expect("probe must attempt the blocked write");
    if let Some(payload) = server_ping {
        // This Ping is read before the following order, causing tungstenite
        // to queue an automatic Pong against the same blocked writer.
        fixture.raw_frame(9, payload).await;
    }
    fixture
        .raw_frame(
            1,
            br#"{"Data":{"OrderNo":"write-blocked","Status":"TRADED"},"Type":"order_alert"}"#,
        )
        .await;
    let update = time::timeout(Duration::from_secs(1), fixture.orders.recv())
        .await
        .expect("read progresses while Ping flush is pending")
        .expect("order broadcast");
    assert_eq!(update.order_no, "write-blocked");
    time::advance(idle_probe::SEND_TIMEOUT).await;
    assert!(matches!(
        fixture.finish().await,
        Err(OrderUpdateConnectionError::LivenessProbeTimeout {
            phase: "sending Ping",
            timeout_secs: 5
        })
    ));
}

#[tokio::test(start_paused = true)]
async fn test_order_idle_probe_write_backpressure_is_bounded_and_does_not_stop_order_reads() {
    assert_blocked_probe_keeps_reading(None).await;
}

#[tokio::test(start_paused = true)]
async fn test_order_idle_probe_inbound_ping_with_blocked_auto_pong_keeps_reads_and_deadline_live() {
    assert_blocked_probe_keeps_reading(Some(b"server-ping")).await;
}

#[tokio::test(start_paused = true)]
async fn test_order_idle_probe_preserves_close_and_auth_failure_paths() {
    for auth_failure in [false, true] {
        let mut fixture = Fixture::new().await;
        fixture.receive_probe().await;
        if auth_failure {
            fixture
                .raw_frame(
                    1,
                    br#"{"errorCode":"DH-901","errorMessage":"fixture token rejected"}"#,
                )
                .await;
            assert!(matches!(
                fixture.finish().await,
                Err(OrderUpdateConnectionError::AuthFailed(_))
            ));
        } else {
            fixture.raw_frame(8, &[]).await;
            assert!(fixture.finish().await.is_ok());
        }
    }
}

#[tokio::test(start_paused = true)]
async fn test_order_idle_probe_cancellation_clears_connected_gauge() {
    let mut fixture = Fixture::new().await;
    fixture.receive_probe().await;
    fixture.session.abort();
    assert!(
        (&mut fixture.session)
            .await
            .expect_err("cancelled session")
            .is_cancelled()
    );
    assert_eq!(f64::from_bits(fixture.active.load(Ordering::SeqCst)), 0.0);
}

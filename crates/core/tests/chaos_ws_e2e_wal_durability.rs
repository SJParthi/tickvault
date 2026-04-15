//! E2E — WS read path + WAL durability against a real tokio
//! WebSocket server.
//!
//! Production scenario protected here: the full chain from a
//! running WebSocket server → tungstenite frame → activity counter
//! bump → WAL append → SIGKILL-equivalent drop → `replay_all` →
//! recovered frame with the exact payload. This is the integration
//! test that proves every Stage-C primitive plays nicely together
//! against a real async runtime and real socket.
//!
//! Why it exists: unit tests inside `connection.rs` cover the
//! private `run_read_loop` method against make_ws_pair. This file
//! covers the WAL durability half of the chain from OUTSIDE the
//! crate, using only the public surface of `WsFrameSpill`, so any
//! subtle refactor that breaks the public contract is caught here.
//!
//! Scenarios:
//!   1. 200 binary frames sent by the mock server land in the WAL
//!      exactly once in FIFO order with exact payload preservation
//!   2. Silence mid-stream does NOT break the WAL append pipeline
//!   3. Server sends a Close frame — WAL is flushed and replay
//!      recovers every frame written BEFORE the close
//!   4. Abrupt TCP drop — WAL records up to the drop survive

#![cfg(test)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{WebSocketStream, accept_async, client_async};

use tickvault_storage::ws_frame_spill::{AppendOutcome, WsFrameSpill, WsType, replay_all};

fn chaos_tmp(tag: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!("tv-e2e-wal-{tag}-{}-{nanos}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create e2e temp dir"); // APPROVED: test
    p
}

fn build_ticker_frame(marker: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 16];
    buf[0] = 2; // RESPONSE_CODE_TICKER
    buf[1..3].copy_from_slice(&16u16.to_le_bytes());
    buf[3] = 2; // NSE_FNO
    buf[4..8].copy_from_slice(&marker.to_le_bytes());
    buf[8..12].copy_from_slice(&100.0_f32.to_le_bytes());
    buf[12..16].copy_from_slice(&(1_700_000_000u32 + marker).to_le_bytes());
    buf
}

async fn spawn_mock_server<F>(handler: F) -> (u16, tokio::task::JoinHandle<anyhow::Result<()>>)
where
    F: FnOnce(
            WebSocketStream<TcpStream>,
        ) -> futures_util::future::BoxFuture<'static, anyhow::Result<()>>
        + Send
        + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock ws");
    let port = listener.local_addr().expect("local_addr").port();
    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let ws = accept_async(stream).await?;
        handler(ws).await
    });
    (port, handle)
}

async fn connect_client(port: u16) -> WebSocketStream<TcpStream> {
    let stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("client connect");
    let url = format!("ws://127.0.0.1:{port}");
    let (ws, _resp) = client_async(url, stream).await.expect("client handshake");
    ws
}

/// Drive the reader side of a WebSocketStream through the same
/// read-loop shape production uses:
///   1. bump activity counter on every Some(Ok(_)) frame
///   2. append binary frames to the real WAL
///   3. exit cleanly on None / Close / Err
async fn drive_reader(
    mut client_ws: WebSocketStream<TcpStream>,
    spill: Arc<WsFrameSpill>,
    activity_counter: Arc<AtomicU64>,
) {
    while let Some(msg) = client_ws.next().await {
        match msg {
            Ok(frame) => {
                activity_counter.fetch_add(1, Ordering::Relaxed);
                if let Message::Binary(bytes) = frame {
                    let outcome = spill.append(WsType::LiveFeed, bytes.to_vec());
                    // Healthy ops: every append must spill durably.
                    assert_eq!(outcome, AppendOutcome::Spilled);
                } else if matches!(frame, Message::Close(_)) {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// **Scenario 1** — 200 binary frames round-trip through the full
/// mock WS → WAL chain. Every frame lands in the WAL exactly once,
/// in FIFO order, with exact payload bytes.
#[tokio::test]
async fn chaos_e2e_200_frames_land_in_wal_in_fifo_order() {
    let dir = chaos_tmp("fifo-200");
    let spill = Arc::new(WsFrameSpill::new(&dir).expect("spill new"));
    let counter = Arc::new(AtomicU64::new(0));

    let (port, server_handle) = spawn_mock_server(|mut server_ws| {
        Box::pin(async move {
            for i in 0..200u32 {
                server_ws
                    .send(Message::Binary(build_ticker_frame(i).into()))
                    .await?;
            }
            server_ws
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Normal,
                    reason: "done".into(),
                })))
                .await?;
            Ok(())
        })
    })
    .await;

    let client_ws = connect_client(port).await;
    drive_reader(client_ws, Arc::clone(&spill), Arc::clone(&counter)).await;

    // Activity counter saw 200 binary + 1 close = 201.
    assert!(
        counter.load(Ordering::Relaxed) >= 200,
        "activity counter must observe every frame"
    );

    // Let the WAL writer thread drain to disk.
    for _ in 0..200 {
        if spill.persisted_count() >= 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(spill);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let recovered = replay_all(&dir).expect("replay_all");
    assert_eq!(recovered.len(), 200, "WAL must contain every frame");

    for (i, rec) in recovered.iter().enumerate() {
        assert_eq!(rec.ws_type, WsType::LiveFeed);
        let sid = u32::from_le_bytes(rec.frame[4..8].try_into().expect("sid slice"));
        assert_eq!(
            sid, i as u32,
            "FIFO order violated at index {i}: security_id = {sid}"
        );
    }

    let _ = server_handle.await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// **Scenario 2** — silence mid-stream does not break the WAL
/// pipeline. 10 frames, 500 ms pause, 10 more. All 20 land durably.
#[tokio::test]
async fn chaos_e2e_silence_window_does_not_break_wal_pipeline() {
    let dir = chaos_tmp("silence");
    let spill = Arc::new(WsFrameSpill::new(&dir).expect("spill new"));
    let counter = Arc::new(AtomicU64::new(0));

    let (port, server_handle) = spawn_mock_server(|mut server_ws| {
        Box::pin(async move {
            for i in 0..10u32 {
                server_ws
                    .send(Message::Binary(build_ticker_frame(i).into()))
                    .await?;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            for i in 10..20u32 {
                server_ws
                    .send(Message::Binary(build_ticker_frame(i).into()))
                    .await?;
            }
            server_ws
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Normal,
                    reason: "done".into(),
                })))
                .await?;
            Ok(())
        })
    })
    .await;

    let client_ws = connect_client(port).await;
    drive_reader(client_ws, Arc::clone(&spill), Arc::clone(&counter)).await;

    for _ in 0..200 {
        if spill.persisted_count() >= 20 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(spill);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let recovered = replay_all(&dir).expect("replay_all");
    assert_eq!(
        recovered.len(),
        20,
        "all 20 frames must survive the silence window"
    );

    let _ = server_handle.await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// **Scenario 3** — abrupt TCP drop. Server sends 5 frames then
/// drops the TcpStream without a close frame. All 5 frames survive
/// in the WAL.
#[tokio::test]
async fn chaos_e2e_tcp_drop_preserves_pre_drop_frames_in_wal() {
    let dir = chaos_tmp("tcp-drop");
    let spill = Arc::new(WsFrameSpill::new(&dir).expect("spill new"));
    let counter = Arc::new(AtomicU64::new(0));

    let (port, server_handle) = spawn_mock_server(|mut server_ws| {
        Box::pin(async move {
            for i in 0..5u32 {
                server_ws
                    .send(Message::Binary(build_ticker_frame(i).into()))
                    .await?;
            }
            // Abrupt drop — no close frame.
            drop(server_ws);
            Ok(())
        })
    })
    .await;

    let client_ws = connect_client(port).await;
    drive_reader(client_ws, Arc::clone(&spill), Arc::clone(&counter)).await;

    for _ in 0..200 {
        if spill.persisted_count() >= 5 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(spill);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let recovered = replay_all(&dir).expect("replay_all");
    assert_eq!(
        recovered.len(),
        5,
        "frames sent before TCP drop must all be in WAL"
    );

    let _ = server_handle.await;
    let _ = std::fs::remove_dir_all(&dir);
}

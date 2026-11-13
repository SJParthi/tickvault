//! Audit B2 findings 6 and 7 (2026-11-13): the order-update dial is bounded and
//! goes through the keepalive TCP connect.

use super::*;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

#[tokio::test]
async fn test_regression_order_update_dial_times_out_on_a_silent_peer() {
    // A peer that accepts TCP and never answers the TLS hello. Before the
    // fix the dial had no bound and this await never returned; the outer
    // 5 s timeout here turns that hang into a test failure.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = tokio::spawn(async move {
        if let Ok((socket, _)) = listener.accept().await {
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(socket);
        }
    });
    let request = format!("wss://127.0.0.1:{port}/")
        .into_client_request()
        .expect("request");
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let connector = build_websocket_tls_connector().expect("tls connector");
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        dial_order_update_socket(request, connector, Duration::from_millis(300)),
    )
    .await;
    server.abort();
    let verdict: Result<String, String> = match outcome {
        Ok(Err(OrderUpdateConnectionError::Connect(msg))) => Ok(msg),
        Ok(Err(other)) => Err(format!("expected a Connect timeout, got {other}")),
        Ok(Ok(_)) => Err("a silent peer must never complete the dial".to_string()),
        Err(_) => {
            Err("the order-update dial is unbounded: it did not return within 5 s".to_string())
        }
    };
    let msg = verdict.expect("order-update dial outcome");
    assert!(msg.contains("timed out"), "unexpected message: {msg}");
}

#[tokio::test]
async fn test_order_update_dial_refused_port_is_a_connect_error() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
    let request = format!("wss://127.0.0.1:{port}/")
        .into_client_request()
        .expect("request");
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let connector = build_websocket_tls_connector().expect("tls connector");
    let outcome = dial_order_update_socket(request, connector, Duration::from_secs(5)).await;
    assert!(matches!(
        outcome,
        Err(OrderUpdateConnectionError::Connect(_))
    ));
}

#[test]
fn test_production_dial_uses_the_main_feed_dial_timeout_and_keepalive() {
    // Source pin: the live dial passes the shared 15 s bound and goes through
    // the keepalive connect, never the library's plain dial.
    let src = include_str!("../order_update_connection.rs");
    let production =
        tickvault_common::source_scan::production_region(src).expect("production region");
    assert!(
        production
            .contains("dial_order_update_socket(request, tls_connector, DIAL_TIMEOUT).await?")
    );
    assert!(production.contains("dial_ws_tls_with_keepalive("));
    assert!(!production.contains("connect_async_tls_with_config("));
    assert_eq!(DIAL_TIMEOUT, Duration::from_secs(15));
}

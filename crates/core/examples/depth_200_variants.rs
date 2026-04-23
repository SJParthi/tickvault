//! Dhan 200-Depth Disconnect Root-Cause Diagnostic (Phase 1)
//!
//! Opens THREE parallel WebSocket connections to the SAME SecurityId 72271
//! at 200-level depth, each with a different URL path / header combination,
//! and logs every frame + disconnect to stderr with a `variant=` label so
//! we can compare which variant stays connected while the others TCP-reset.
//!
//! This is an isolated Cargo example — it does NOT touch the production
//! depth connection path. Run it like:
//!
//! ```bash
//! export DHAN_CLIENT_ID='1106656882'
//! export DHAN_ACCESS_TOKEN='<fresh JWT from web.dhan.co Profile>'
//! cd crates/core
//! cargo run --release --example depth_200_variants
//! ```
//!
//! Runs for `DEFAULT_TEST_DURATION_SECS` seconds then exits with a summary
//! table (frames received, disconnects, last error per variant).
//!
//! ## Variants tested
//!
//! | Variant | URL path | Headers |
//! |---------|----------|---------|
//! | A | `/` (root, matches Dhan Python SDK) | tungstenite defaults |
//! | B | `/twohundreddepth` (our current prod) | tungstenite defaults |
//! | C | `/` (root) | User-Agent mimicking Python `websockets/16.0` |
//!
//! ## Expected signal
//!
//! - A works, B disconnects → URL path is the bug. Simple fix.
//! - A & B disconnect, C works → header fingerprint. Add User-Agent.
//! - All three disconnect → go to Phase 2 (TLS backend, alternate WS lib).
//!
//! See `.claude/plans/active-plan.md` for the full Phase 1 → 3 plan.

use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{Connector, connect_async_tls_with_config};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

// Test parameters — intentionally NOT loaded from AppConfig so the example
// runs standalone without needing the full tickvault config stack.
const TEST_SECURITY_ID: &str = "72271";
const TEST_EXCHANGE_SEGMENT: &str = "NSE_FNO";
const DHAN_200_DEPTH_HOST: &str = "full-depth-api.dhan.co";
const DEFAULT_TEST_DURATION_SECS: u64 = 1800; // 30 min
const CONNECT_TIMEOUT_SECS: u64 = 15;
const PROGRESS_LOG_INTERVAL_SECS: u64 = 60;

/// User-Agent emitted by Python `websockets` library 16.0 (what the Dhan
/// Python SDK uses). Captured via `curl -v` + Python source inspection.
/// If variant C works and A/B fail, adding this UA to our prod client is
/// likely the fix.
const PYTHON_WEBSOCKETS_UA: &str = "Python/3.12 websockets/16.0";

#[derive(Debug, Clone, Copy)]
enum Variant {
    /// Root path `/`, tungstenite default headers — matches Dhan Python SDK URL.
    A,
    /// Explicit `/twohundreddepth` path, tungstenite default headers — current prod.
    B,
    /// Root path `/` + Python-websockets User-Agent header.
    C,
}

impl Variant {
    fn label(self) -> &'static str {
        match self {
            Self::A => "A_root_default_headers",
            Self::B => "B_twohundreddepth_default_headers",
            Self::C => "C_root_python_ua",
        }
    }

    fn url_path(self) -> &'static str {
        match self {
            Self::A | Self::C => "/",
            Self::B => "/twohundreddepth",
        }
    }

    fn add_custom_headers(self) -> bool {
        matches!(self, Self::C)
    }
}

/// Per-variant live counters + last-seen disconnect reason. Shared across
/// the connect/subscribe/read loop and the 30-min summary printer.
#[derive(Debug, Default)]
struct VariantStats {
    frames_received: AtomicU64,
    bytes_received: AtomicU64,
    disconnects: AtomicU64,
    subscribe_acks: AtomicU64,
    last_disconnect_reason: parking_lot_like_cell::Cell<Option<String>>,
}

// Tiny inline cell wrapper so we don't pull in `parking_lot` just for this.
// Uses `tokio::sync::Mutex` instead so it's `Send`.
mod parking_lot_like_cell {
    use tokio::sync::Mutex;

    #[derive(Debug, Default)]
    pub struct Cell<T> {
        inner: Mutex<T>,
    }

    impl<T: Clone> Cell<T> {
        pub async fn set(&self, value: T) {
            *self.inner.lock().await = value;
        }
    }
}

fn build_subscribe_message() -> String {
    // 200-level depth uses FLAT subscribe JSON (NOT the InstrumentList form
    // used for 20-level). RequestCode 23 = subscribe.
    format!(
        r#"{{"RequestCode":23,"ExchangeSegment":"{TEST_EXCHANGE_SEGMENT}","SecurityId":"{TEST_SECURITY_ID}"}}"#
    )
}

/// Build a rustls TLS connector with HTTP/1.1 ALPN — matches what the
/// production `build_websocket_tls_connector` does. Duplicated here so
/// the example does not depend on the private `websocket::tls` module.
fn build_tls_connector() -> Result<Connector, String> {
    let mut root_store = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    let (added, _ignored) = root_store.add_parsable_certificates(native.certs);
    if added == 0 {
        return Err("no native root CA certificates found".into());
    }
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Connector::Rustls(Arc::new(config)))
}

async fn run_variant(
    variant: Variant,
    client_id: String,
    token_value: String,
    stats: Arc<VariantStats>,
) {
    let url = format!(
        "wss://{host}{path}?token={token}&clientId={client_id}&authType=2",
        host = DHAN_200_DEPTH_HOST,
        path = variant.url_path(),
        token = token_value,
    );

    info!(
        variant = variant.label(),
        url_path = variant.url_path(),
        "connecting"
    );

    let request_result = url.as_str().into_client_request();
    let mut request = match request_result {
        Ok(r) => r,
        Err(err) => {
            error!(
                variant = variant.label(),
                ?err,
                "into_client_request failed"
            );
            return;
        }
    };

    if variant.add_custom_headers() {
        if let Ok(ua) = HeaderValue::from_str(PYTHON_WEBSOCKETS_UA) {
            request.headers_mut().insert("User-Agent", ua);
        }
    }

    let connector = match build_tls_connector() {
        Ok(c) => c,
        Err(err) => {
            error!(variant = variant.label(), err, "TLS connector build failed");
            return;
        }
    };

    let connect_fut = connect_async_tls_with_config(request, None, false, Some(connector));
    let ws_result = match timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), connect_fut).await {
        Ok(Ok(tuple)) => tuple,
        Ok(Err(err)) => {
            error!(variant = variant.label(), ?err, "connect failed");
            stats.disconnects.fetch_add(1, Ordering::Relaxed);
            stats
                .last_disconnect_reason
                .set(Some(format!("connect error: {err}")))
                .await;
            return;
        }
        Err(_) => {
            error!(
                variant = variant.label(),
                seconds = CONNECT_TIMEOUT_SECS,
                "connect timed out"
            );
            stats.disconnects.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    let (ws_stream, response) = ws_result;
    info!(
        variant = variant.label(),
        status = ?response.status(),
        "connected — sending subscribe"
    );

    let (mut write, mut read) = ws_stream.split();

    let subscribe = build_subscribe_message();
    if let Err(err) = write.send(Message::Text(subscribe.into())).await {
        error!(variant = variant.label(), ?err, "subscribe send failed");
        stats.disconnects.fetch_add(1, Ordering::Relaxed);
        return;
    }
    stats.subscribe_acks.fetch_add(1, Ordering::Relaxed);

    while let Some(frame_result) = read.next().await {
        match frame_result {
            Ok(Message::Binary(bytes)) => {
                stats.frames_received.fetch_add(1, Ordering::Relaxed);
                stats
                    .bytes_received
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                // Log at DEBUG so we don't spam stderr — set RUST_LOG=debug to see.
                tracing::debug!(
                    variant = variant.label(),
                    bytes = bytes.len(),
                    "binary frame"
                );
            }
            Ok(Message::Text(text)) => {
                tracing::debug!(variant = variant.label(), %text, "text frame");
            }
            Ok(Message::Close(frame)) => {
                warn!(variant = variant.label(), ?frame, "server sent close frame");
                stats.disconnects.fetch_add(1, Ordering::Relaxed);
                stats
                    .last_disconnect_reason
                    .set(Some(format!("close frame: {frame:?}")))
                    .await;
                break;
            }
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {
                // No-op — tungstenite auto-pongs pings.
            }
            Err(err) => {
                error!(variant = variant.label(), ?err, "read error");
                stats.disconnects.fetch_add(1, Ordering::Relaxed);
                stats
                    .last_disconnect_reason
                    .set(Some(format!("read error: {err}")))
                    .await;
                break;
            }
        }
    }

    warn!(variant = variant.label(), "read loop exited");
}

#[tokio::main]
async fn main() -> Result<(), String> {
    // Install aws-lc-rs crypto provider — same as production main.rs.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,depth_200_variants=info")),
        )
        .with_target(false)
        .init();

    let client_id =
        env::var("DHAN_CLIENT_ID").map_err(|_| "DHAN_CLIENT_ID env var not set".to_string())?;
    let token_value = env::var("DHAN_ACCESS_TOKEN")
        .map_err(|_| "DHAN_ACCESS_TOKEN env var not set".to_string())?;

    let test_duration_secs: u64 = env::var("TEST_DURATION_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_TEST_DURATION_SECS);

    info!(
        test_duration_secs,
        security_id = TEST_SECURITY_ID,
        segment = TEST_EXCHANGE_SEGMENT,
        "starting 3-variant 200-depth diagnostic — Ctrl+C to stop early"
    );

    let stats_a = Arc::new(VariantStats::default());
    let stats_b = Arc::new(VariantStats::default());
    let stats_c = Arc::new(VariantStats::default());

    let client_id_a = client_id.clone();
    let token_a = token_value.clone();
    let stats_a_task = Arc::clone(&stats_a);
    let task_a = tokio::spawn(async move {
        run_variant(Variant::A, client_id_a, token_a, stats_a_task).await;
    });

    let client_id_b = client_id.clone();
    let token_b = token_value.clone();
    let stats_b_task = Arc::clone(&stats_b);
    let task_b = tokio::spawn(async move {
        run_variant(Variant::B, client_id_b, token_b, stats_b_task).await;
    });

    let stats_c_task = Arc::clone(&stats_c);
    let task_c = tokio::spawn(async move {
        run_variant(Variant::C, client_id, token_value, stats_c_task).await;
    });

    // Periodic progress print every 60s so operator can eyeball live status.
    let stats_a_monitor = Arc::clone(&stats_a);
    let stats_b_monitor = Arc::clone(&stats_b);
    let stats_c_monitor = Arc::clone(&stats_c);
    let monitor = tokio::spawn(async move {
        let mut ticks = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(PROGRESS_LOG_INTERVAL_SECS)).await;
            ticks += 1;
            info!(
                minute = ticks,
                a_frames = stats_a_monitor.frames_received.load(Ordering::Relaxed),
                a_disc = stats_a_monitor.disconnects.load(Ordering::Relaxed),
                b_frames = stats_b_monitor.frames_received.load(Ordering::Relaxed),
                b_disc = stats_b_monitor.disconnects.load(Ordering::Relaxed),
                c_frames = stats_c_monitor.frames_received.load(Ordering::Relaxed),
                c_disc = stats_c_monitor.disconnects.load(Ordering::Relaxed),
                "progress"
            );
        }
    });

    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(test_duration_secs)) => {
            info!(test_duration_secs, "test duration elapsed — shutting down");
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Ctrl+C received — shutting down");
        }
    }

    monitor.abort();
    task_a.abort();
    task_b.abort();
    task_c.abort();

    // Final summary.
    let a_frames = stats_a.frames_received.load(Ordering::Relaxed);
    let a_disc = stats_a.disconnects.load(Ordering::Relaxed);
    let a_bytes = stats_a.bytes_received.load(Ordering::Relaxed);

    let b_frames = stats_b.frames_received.load(Ordering::Relaxed);
    let b_disc = stats_b.disconnects.load(Ordering::Relaxed);
    let b_bytes = stats_b.bytes_received.load(Ordering::Relaxed);

    let c_frames = stats_c.frames_received.load(Ordering::Relaxed);
    let c_disc = stats_c.disconnects.load(Ordering::Relaxed);
    let c_bytes = stats_c.bytes_received.load(Ordering::Relaxed);

    info!("===== SUMMARY ({test_duration_secs}s run) =====");
    info!(
        variant = "A_root_default_headers",
        frames = a_frames,
        bytes = a_bytes,
        disconnects = a_disc
    );
    info!(
        variant = "B_twohundreddepth_default_headers",
        frames = b_frames,
        bytes = b_bytes,
        disconnects = b_disc
    );
    info!(
        variant = "C_root_python_ua",
        frames = c_frames,
        bytes = c_bytes,
        disconnects = c_disc
    );

    info!("=====");
    info!("If ONE variant has high frames and zero disconnects, THAT is the fix.");
    info!("If ALL variants show frames=0 and disconnects>=1, go to Phase 2.");

    Ok(())
}

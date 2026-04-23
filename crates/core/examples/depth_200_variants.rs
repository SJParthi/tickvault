//! Dhan 200-Depth Disconnect Root-Cause Diagnostic (Phase 1 — FULL MATRIX)
//!
//! Runs EIGHT variants sequentially against the SAME SecurityId 72271 at
//! 200-level depth. Each variant gets its own `PER_VARIANT_DURATION_SECS`
//! window, then moves to the next. Sequential execution (not parallel)
//! avoids hitting Dhan's 5-connection-per-account 200-depth cap.
//!
//! This is an isolated Cargo example — it does NOT touch the production
//! depth connection path. Run:
//!
//! ```bash
//! export DHAN_CLIENT_ID='1106656882'
//! export DHAN_ACCESS_TOKEN='<fresh JWT from web.dhan.co>'
//! cd crates/core
//! cargo run --release --example depth_200_variants
//! ```
//!
//! Default total run: 8 variants × 180s = ~24 min. Override with
//! `PER_VARIANT_DURATION_SECS=60` for a faster smoke test.
//!
//! ## Variant matrix
//!
//! | # | URL path | UA | ALPN | Tests |
//! |---|----------|----|----|----|
//! | A | `/`                  | default | http/1.1 | URL root vs explicit |
//! | B | `/twohundreddepth`   | default | http/1.1 | Current prod baseline |
//! | C | `/`                  | Python  | http/1.1 | Root + fingerprint |
//! | D | `/twohundreddepth`   | Python  | http/1.1 | Explicit + fingerprint |
//! | E | `/`                  | default | none     | Root + no ALPN |
//! | F | `/`                  | Browser | http/1.1 | Extra browser headers |
//! | G | `/twohundreddepth`   | default | none     | Explicit + no ALPN |
//! | H | `/`                  | Python  | none     | Root + Python + no ALPN |
//!
//! ## Interpreting results
//!
//! Look at the final summary table — ANY variant with `disconnects=0` AND
//! `frames>0` is the configuration that works. Apply that combination to
//! `crates/core/src/websocket/depth_connection.rs` in prod.
//!
//! See `.claude/plans/active-plan.md` for the full plan.

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

// Test parameters — standalone so the example doesn't need AppConfig stack.
const TEST_SECURITY_ID: &str = "72271";
const TEST_EXCHANGE_SEGMENT: &str = "NSE_FNO";
const DHAN_200_DEPTH_HOST: &str = "full-depth-api.dhan.co";
const DEFAULT_PER_VARIANT_DURATION_SECS: u64 = 180; // 3 min each × 8 = ~24 min total
const CONNECT_TIMEOUT_SECS: u64 = 15;
const PROGRESS_LOG_INTERVAL_SECS: u64 = 30;

/// User-Agent emitted by Python `websockets` library 16.0 (what Dhan Python
/// SDK uses). If a Python-UA variant works and default-UA doesn't, adding
/// this UA to our prod WebSocket request is the fix.
const PYTHON_WEBSOCKETS_UA: &str = "Python/3.12 websockets/16.0";

/// Chrome-like User-Agent plus a few headers browsers normally send.
/// If a "looks like a browser" variant works, the issue is fingerprinting.
const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
const BROWSER_ORIGIN_SCHEME: &str = "https";
const BROWSER_ORIGIN_HOST: &str = "web.dhan.co";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UaFlavor {
    /// No custom User-Agent — tungstenite default (`tungstenite-rs/<version>`).
    Default,
    /// Match Python `websockets/16.0` library.
    Python,
    /// Match Chrome browser + Origin header.
    Browser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AlpnMode {
    /// Force `http/1.1` ALPN — matches current prod `build_websocket_tls_connector`.
    Http11,
    /// No ALPN — TLS handshake does not advertise any protocol preference.
    None,
}

#[derive(Debug, Clone, Copy)]
struct Variant {
    label: &'static str,
    url_path: &'static str,
    ua: UaFlavor,
    alpn: AlpnMode,
}

const VARIANTS: [Variant; 8] = [
    Variant {
        label: "A_root_default_alpn11",
        url_path: "/",
        ua: UaFlavor::Default,
        alpn: AlpnMode::Http11,
    },
    Variant {
        label: "B_twohundred_default_alpn11",
        url_path: "/twohundreddepth",
        ua: UaFlavor::Default,
        alpn: AlpnMode::Http11,
    },
    Variant {
        label: "C_root_python_alpn11",
        url_path: "/",
        ua: UaFlavor::Python,
        alpn: AlpnMode::Http11,
    },
    Variant {
        label: "D_twohundred_python_alpn11",
        url_path: "/twohundreddepth",
        ua: UaFlavor::Python,
        alpn: AlpnMode::Http11,
    },
    Variant {
        label: "E_root_default_noalpn",
        url_path: "/",
        ua: UaFlavor::Default,
        alpn: AlpnMode::None,
    },
    Variant {
        label: "F_root_browser_alpn11",
        url_path: "/",
        ua: UaFlavor::Browser,
        alpn: AlpnMode::Http11,
    },
    Variant {
        label: "G_twohundred_default_noalpn",
        url_path: "/twohundreddepth",
        ua: UaFlavor::Default,
        alpn: AlpnMode::None,
    },
    Variant {
        label: "H_root_python_noalpn",
        url_path: "/",
        ua: UaFlavor::Python,
        alpn: AlpnMode::None,
    },
];

#[derive(Debug, Default)]
struct VariantStats {
    frames_received: AtomicU64,
    bytes_received: AtomicU64,
    disconnects: AtomicU64,
    subscribe_acks: AtomicU64,
    last_disconnect_reason: tokio::sync::Mutex<Option<String>>,
}

fn build_subscribe_message() -> String {
    // 200-level depth uses FLAT subscribe JSON (not InstrumentList).
    // RequestCode 23 = subscribe.
    format!(
        r#"{{"RequestCode":23,"ExchangeSegment":"{TEST_EXCHANGE_SEGMENT}","SecurityId":"{TEST_SECURITY_ID}"}}"#
    )
}

/// Build a rustls TLS connector. `alpn_http11=true` forces `http/1.1`
/// (matches prod `build_websocket_tls_connector`). `false` disables ALPN
/// so the handshake advertises no protocol preference.
fn build_tls_connector(alpn_http11: bool) -> Result<Connector, String> {
    let mut root_store = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    let (added, _ignored) = root_store.add_parsable_certificates(native.certs);
    if added == 0 {
        return Err("no native root CA certificates found".into());
    }
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    if alpn_http11 {
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
    }
    Ok(Connector::Rustls(Arc::new(config)))
}

async fn run_one_variant(
    variant: Variant,
    client_id: &str,
    token: &str,
    stats: Arc<VariantStats>,
    budget_secs: u64,
) {
    let url = format!(
        "wss://{host}{path}?token={token}&clientId={client_id}&authType=2",
        host = DHAN_200_DEPTH_HOST,
        path = variant.url_path,
    );

    info!(
        variant = variant.label,
        url_path = variant.url_path,
        ua = ?variant.ua,
        alpn = ?variant.alpn,
        budget_secs,
        "start"
    );

    let request_result = url.as_str().into_client_request();
    let mut request = match request_result {
        Ok(r) => r,
        Err(err) => {
            error!(variant = variant.label, ?err, "into_client_request failed");
            return;
        }
    };

    match variant.ua {
        UaFlavor::Default => {}
        UaFlavor::Python => {
            if let Ok(ua) = HeaderValue::from_str(PYTHON_WEBSOCKETS_UA) {
                request.headers_mut().insert("User-Agent", ua);
            }
        }
        UaFlavor::Browser => {
            if let Ok(ua) = HeaderValue::from_str(BROWSER_UA) {
                request.headers_mut().insert("User-Agent", ua);
            }
            let origin = format!("{BROWSER_ORIGIN_SCHEME}://{BROWSER_ORIGIN_HOST}");
            if let Ok(origin_header) = HeaderValue::from_str(&origin) {
                request.headers_mut().insert("Origin", origin_header);
            }
            if let Ok(pragma) = HeaderValue::from_str("no-cache") {
                request.headers_mut().insert("Pragma", pragma);
            }
            if let Ok(cc) = HeaderValue::from_str("no-cache") {
                request.headers_mut().insert("Cache-Control", cc);
            }
        }
    }

    let alpn_http11 = matches!(variant.alpn, AlpnMode::Http11);
    let connector = match build_tls_connector(alpn_http11) {
        Ok(c) => c,
        Err(err) => {
            error!(variant = variant.label, err, "TLS connector build failed");
            return;
        }
    };

    let connect_fut = connect_async_tls_with_config(request, None, false, Some(connector));
    let ws_result = match timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), connect_fut).await {
        Ok(Ok(tuple)) => tuple,
        Ok(Err(err)) => {
            error!(variant = variant.label, ?err, "connect failed");
            stats.disconnects.fetch_add(1, Ordering::Relaxed);
            *stats.last_disconnect_reason.lock().await = Some(format!("connect error: {err}"));
            return;
        }
        Err(_) => {
            error!(
                variant = variant.label,
                seconds = CONNECT_TIMEOUT_SECS,
                "connect timed out"
            );
            stats.disconnects.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    let (ws_stream, response) = ws_result;
    info!(
        variant = variant.label,
        status = ?response.status(),
        "connected — sending subscribe"
    );

    let (mut write, mut read) = ws_stream.split();

    let subscribe = build_subscribe_message();
    if let Err(err) = write.send(Message::Text(subscribe.into())).await {
        error!(variant = variant.label, ?err, "subscribe send failed");
        stats.disconnects.fetch_add(1, Ordering::Relaxed);
        return;
    }
    stats.subscribe_acks.fetch_add(1, Ordering::Relaxed);

    // Read frames until budget elapses OR disconnect.
    let deadline = tokio::time::sleep(Duration::from_secs(budget_secs));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => {
                info!(variant = variant.label, "budget elapsed — moving to next variant");
                break;
            }
            maybe_frame = read.next() => {
                match maybe_frame {
                    Some(Ok(Message::Binary(bytes))) => {
                        stats.frames_received.fetch_add(1, Ordering::Relaxed);
                        stats
                            .bytes_received
                            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    }
                    Some(Ok(Message::Text(text))) => {
                        tracing::debug!(variant = variant.label, %text, "text frame");
                    }
                    Some(Ok(Message::Close(frame))) => {
                        warn!(variant = variant.label, ?frame, "server sent close frame");
                        stats.disconnects.fetch_add(1, Ordering::Relaxed);
                        *stats.last_disconnect_reason.lock().await =
                            Some(format!("close frame: {frame:?}"));
                        break;
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {
                        // no-op
                    }
                    Some(Err(err)) => {
                        error!(variant = variant.label, ?err, "read error");
                        stats.disconnects.fetch_add(1, Ordering::Relaxed);
                        *stats.last_disconnect_reason.lock().await =
                            Some(format!("read error: {err}"));
                        break;
                    }
                    None => {
                        warn!(variant = variant.label, "stream ended (None)");
                        stats.disconnects.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                }
            }
        }
    }

    info!(
        variant = variant.label,
        frames = stats.frames_received.load(Ordering::Relaxed),
        bytes = stats.bytes_received.load(Ordering::Relaxed),
        disconnects = stats.disconnects.load(Ordering::Relaxed),
        "variant done"
    );
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

    let per_variant_secs: u64 = env::var("PER_VARIANT_DURATION_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PER_VARIANT_DURATION_SECS);

    let total_secs = per_variant_secs * (VARIANTS.len() as u64);
    info!(
        n_variants = VARIANTS.len(),
        per_variant_secs,
        total_secs,
        security_id = TEST_SECURITY_ID,
        segment = TEST_EXCHANGE_SEGMENT,
        "starting sequential depth-200 diagnostic"
    );

    // Build one stats object per variant.
    let mut stats_vec: Vec<Arc<VariantStats>> = Vec::with_capacity(VARIANTS.len());
    for _ in 0..VARIANTS.len() {
        stats_vec.push(Arc::new(VariantStats::default()));
    }

    // Background progress logger — prints a compact row every 30s.
    let stats_for_progress = stats_vec.iter().map(Arc::clone).collect::<Vec<_>>();
    let progress_handle = tokio::spawn(async move {
        let mut ticks = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(PROGRESS_LOG_INTERVAL_SECS)).await;
            ticks += 1;
            for (idx, s) in stats_for_progress.iter().enumerate() {
                info!(
                    tick = ticks,
                    variant = VARIANTS[idx].label,
                    frames = s.frames_received.load(Ordering::Relaxed),
                    disc = s.disconnects.load(Ordering::Relaxed),
                    "progress"
                );
            }
        }
    });

    // Run variants sequentially — one at a time, each with its own budget.
    let run_handle = tokio::spawn({
        let client_id = client_id.clone();
        let token_value = token_value.clone();
        let stats_vec = stats_vec.iter().map(Arc::clone).collect::<Vec<_>>();
        async move {
            for (idx, variant) in VARIANTS.iter().enumerate() {
                run_one_variant(
                    *variant,
                    &client_id,
                    &token_value,
                    Arc::clone(&stats_vec[idx]),
                    per_variant_secs,
                )
                .await;
            }
        }
    });

    tokio::select! {
        _ = run_handle => {
            info!("all variants completed");
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Ctrl+C — early stop");
        }
    }

    progress_handle.abort();

    // Collect results for both on-screen summary and JSON/markdown output files.
    let mut results: Vec<VariantResult> = Vec::with_capacity(VARIANTS.len());
    for (idx, variant) in VARIANTS.iter().enumerate() {
        let s = &stats_vec[idx];
        let frames = s.frames_received.load(Ordering::Relaxed);
        let bytes = s.bytes_received.load(Ordering::Relaxed);
        let disc = s.disconnects.load(Ordering::Relaxed);
        let last_reason = s
            .last_disconnect_reason
            .lock()
            .await
            .clone()
            .unwrap_or_default();
        results.push(VariantResult {
            label: variant.label,
            url_path: variant.url_path,
            ua: format!("{:?}", variant.ua),
            alpn: format!("{:?}", variant.alpn),
            frames,
            bytes,
            disconnects: disc,
            last_disconnect_reason: last_reason,
        });
    }

    // Final summary.
    info!("===== SUMMARY =====");
    for r in &results {
        info!(
            variant = r.label,
            frames = r.frames,
            bytes = r.bytes,
            disconnects = r.disconnects,
            last_disconnect = %r.last_disconnect_reason,
            "summary"
        );
    }
    info!("===================");
    let winner = results
        .iter()
        .find(|r| r.frames > 0 && r.disconnects == 0)
        .map(|r| r.label);
    if let Some(label) = winner {
        info!(winner = label, "FIX IDENTIFIED");
    } else {
        info!(
            "NO CLEAR WINNER — all variants disconnected or received no frames. \
             Consider Phase 2 (native-tls, fastwebsockets) or escalate to Dhan support."
        );
    }

    // Write machine-readable results so the analysis / Dhan email scripts can
    // consume them without re-parsing tracing logs.
    if let Err(err) = write_results_json(&results).await {
        error!(?err, "failed to write JSON results file");
    }
    if let Err(err) = write_results_markdown(&results).await {
        error!(?err, "failed to write markdown results file");
    }

    Ok(())
}

#[derive(Debug)]
struct VariantResult {
    label: &'static str,
    url_path: &'static str,
    ua: String,
    alpn: String,
    frames: u64,
    bytes: u64,
    disconnects: u64,
    last_disconnect_reason: String,
}

/// Escape a string for embedding in a JSON literal. Handles the minimum
/// required by RFC 8259 section 7: quote, backslash, control chars <0x20.
fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

async fn write_results_json(results: &[VariantResult]) -> std::io::Result<()> {
    let path = env::var("RESULTS_JSON_PATH")
        .unwrap_or_else(|_| "/tmp/depth_200_variants_results.json".to_string());

    let mut body = String::from("{\n  \"results\": [\n");
    for (i, r) in results.iter().enumerate() {
        body.push_str("    {\n");
        body.push_str(&format!("      \"label\": \"{}\",\n", json_escape(r.label)));
        body.push_str(&format!(
            "      \"url_path\": \"{}\",\n",
            json_escape(r.url_path)
        ));
        body.push_str(&format!("      \"ua\": \"{}\",\n", json_escape(&r.ua)));
        body.push_str(&format!("      \"alpn\": \"{}\",\n", json_escape(&r.alpn)));
        body.push_str(&format!("      \"frames\": {},\n", r.frames));
        body.push_str(&format!("      \"bytes\": {},\n", r.bytes));
        body.push_str(&format!("      \"disconnects\": {},\n", r.disconnects));
        body.push_str(&format!(
            "      \"last_disconnect_reason\": \"{}\"\n",
            json_escape(&r.last_disconnect_reason)
        ));
        body.push_str(if i + 1 == results.len() {
            "    }\n"
        } else {
            "    },\n"
        });
    }
    let winner = results
        .iter()
        .find(|r| r.frames > 0 && r.disconnects == 0)
        .map(|r| r.label);
    body.push_str("  ],\n  \"winner\": ");
    match winner {
        Some(label) => body.push_str(&format!("\"{}\"", json_escape(label))),
        None => body.push_str("null"),
    }
    body.push_str("\n}\n");

    tokio::fs::write(&path, body).await?;
    info!(path = %path, "wrote JSON results");
    Ok(())
}

async fn write_results_markdown(results: &[VariantResult]) -> std::io::Result<()> {
    let path = env::var("RESULTS_MD_PATH")
        .unwrap_or_else(|_| "/tmp/depth_200_variants_results.md".to_string());

    let mut body = String::from("# Depth 200 Variant Test Results\n\n");
    body.push_str(&format!(
        "Run: {} UTC, SecurityId {}, segment {}\n\n",
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S"),
        TEST_SECURITY_ID,
        TEST_EXCHANGE_SEGMENT
    ));
    body.push_str("| Variant | URL Path | UA | ALPN | Frames | Bytes | Disconnects | Last Disconnect Reason |\n");
    body.push_str("|---|---|---|---|---|---|---|---|\n");
    for r in results {
        body.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
            r.label,
            r.url_path,
            r.ua,
            r.alpn,
            r.frames,
            r.bytes,
            r.disconnects,
            if r.last_disconnect_reason.is_empty() {
                "n/a".to_string()
            } else {
                r.last_disconnect_reason.replace('|', "\\|")
            }
        ));
    }
    let winner = results.iter().find(|r| r.frames > 0 && r.disconnects == 0);
    body.push_str("\n## Verdict\n\n");
    match winner {
        Some(w) => body.push_str(&format!(
            "**FIX IDENTIFIED**: variant `{}` stayed connected and received {} frames.\n\
             Apply its `(url_path={}, ua={}, alpn={})` combo to production.\n",
            w.label, w.frames, w.url_path, w.ua, w.alpn
        )),
        None => body.push_str(
            "**NO CLEAR WINNER**: all variants disconnected or received no frames.\n\
             Proceed to Phase 2 (TLS/WS library variants) or escalate to Dhan support \
             with `docs/dhan-support/2026-04-24-depth-200-variant-test-results.md`.\n",
        ),
    }

    tokio::fs::write(&path, body).await?;
    info!(path = %path, "wrote markdown results");
    Ok(())
}

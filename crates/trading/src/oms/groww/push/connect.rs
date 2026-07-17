//! NATS `INFO` → `CONNECT` handshake pieces for the Groww order/position
//! push channel — restored byte-faithful (2026-07-16, order-push Stage B)
//! from the retired live-feed transport
//! (`dd7eaa5e^:crates/core/src/feed/groww/native/connect.rs`). The two pure
//! halves of the auth exchange:
//!
//! 1. [`extract_nonce`] — pull the signing `nonce` out of the server `INFO`
//!    JSON (delivered by [`super::nats::NatsServerOp::Info`]).
//! 2. [`build_connect_frame`] — the `CONNECT {json}\r\n` line carrying the
//!    socket JWT + the base64url ed25519 nonce signature.
//!
//! ## CONNECT field choices (live-probe risk #1 mitigation)
//!
//! The #1 unknown of the native client is whether Groww's NATS server accepts
//! a non-nats-py client. We therefore mirror nats-py's CONNECT surface
//! (`lang`/`version` matching the SDK's pinned `nats-py ^2.9.0` — **Assumed**:
//! server-side acceptance is only provable by the first live probe), and we
//! deliberately advertise `headers: false` + `no_responders: false` so the
//! server delivers plain `MSG` frames — our framing parser
//! ([`super::nats::parse_frame`]) intentionally does not speak
//! `HMSG`.
//!
//! SECURITY: the built frame CONTAINS the socket JWT, so it is returned as a
//! [`SecretString`] — exposed only at the WebSocket send site, never logged,
//! never `Debug`-printed.

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

/// `lang` we advertise in CONNECT — mirrors the official SDK's runtime
/// (nats-py reports `python3`). Assumed-compatible until the first live probe.
pub const CONNECT_LANG: &str = "python3";
/// `version` we advertise — the SDK's pinned nats-py minor (`^2.9.0`).
pub const CONNECT_VERSION: &str = "2.9.0";

/// The subset of the server `INFO` JSON we need.
#[derive(Debug, Deserialize)]
struct ServerInfo {
    nonce: Option<String>,
}

/// Extract the auth `nonce` from the raw `INFO` JSON bytes. `None` when the
/// JSON is malformed OR carries no nonce (a nonce-less INFO means the server
/// did not request nkey auth — the caller treats that as an auth-path error
/// for Groww, whose feed always nonces).
#[must_use]
pub fn extract_nonce(info_json: &[u8]) -> Option<String> {
    serde_json::from_slice::<ServerInfo>(info_json)
        .ok()
        .and_then(|info| info.nonce)
}

/// The CONNECT payload — serialized with serde so the JWT is JSON-escaped
/// correctly no matter its contents.
#[derive(Serialize)]
struct ConnectPayload<'a> {
    verbose: bool,
    pedantic: bool,
    tls_required: bool,
    lang: &'static str,
    version: &'static str,
    protocol: u8,
    headers: bool,
    no_responders: bool,
    jwt: &'a str,
    sig: &'a str,
}

/// Build the full `CONNECT {json}\r\n` wire line.
///
/// Returns a [`SecretString`] because the line embeds the socket JWT — expose
/// it ONLY to write it to the socket.
#[must_use]
pub fn build_connect_frame(socket_jwt: &SecretString, sig_b64url: &str) -> SecretString {
    let payload = ConnectPayload {
        verbose: false,
        pedantic: false,
        tls_required: false,
        lang: CONNECT_LANG,
        version: CONNECT_VERSION,
        protocol: 1,
        headers: false,
        no_responders: false,
        jwt: socket_jwt.expose_secret(),
        sig: sig_b64url,
    };
    // serde_json::to_string on a plain struct of strings/bools/u8 cannot fail;
    // degrade to an empty CONNECT (server rejects → auth error path) rather
    // than panicking in prod.
    let json = serde_json::to_string(&payload).unwrap_or_default();
    let mut frame = String::with_capacity(8 + json.len() + 2);
    frame.push_str("CONNECT ");
    frame.push_str(&json);
    frame.push_str("\r\n");
    SecretString::from(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_nonce_present() {
        let info = br#"{"server_id":"x","nonce":"AXf29GJwrGY0Cq","max_payload":1048576}"#;
        assert_eq!(extract_nonce(info).as_deref(), Some("AXf29GJwrGY0Cq"));
    }

    #[test]
    fn test_extract_nonce_absent_or_garbage() {
        assert_eq!(extract_nonce(br#"{"server_id":"x"}"#), None);
        assert_eq!(extract_nonce(b"not json at all"), None);
        assert_eq!(extract_nonce(b""), None);
    }

    /// The frame is one `CONNECT {json}\r\n` line whose JSON round-trips with
    /// the jwt + sig intact, headers/no_responders OFF (our parser has no
    /// HMSG arm), protocol 1, and non-verbose.
    #[test]
    fn test_build_connect_frame_shape() {
        let jwt = SecretString::from("eyJhbGciOiJlZDI1NTE5In0.payload.sig");
        let frame = build_connect_frame(&jwt, "c2lnbmF0dXJl");
        let raw = frame.expose_secret();
        assert!(raw.starts_with("CONNECT {"));
        assert!(raw.ends_with("}\r\n"));
        assert_eq!(raw.matches("\r\n").count(), 1, "exactly one line");

        let json: serde_json::Value =
            serde_json::from_str(&raw["CONNECT ".len()..raw.len() - 2]).expect("valid json");
        assert_eq!(json["jwt"], "eyJhbGciOiJlZDI1NTE5In0.payload.sig");
        assert_eq!(json["sig"], "c2lnbmF0dXJl");
        assert_eq!(json["verbose"], false);
        assert_eq!(json["headers"], false, "HMSG must never be negotiated");
        assert_eq!(json["no_responders"], false);
        assert_eq!(json["protocol"], 1);
        assert_eq!(json["lang"], CONNECT_LANG);
        assert_eq!(json["version"], CONNECT_VERSION);
    }

    /// A JWT containing JSON-hostile characters is escaped, not spliced.
    #[test]
    fn test_build_connect_frame_escapes_hostile_jwt() {
        let jwt = SecretString::from(r#"evil"jwt\with{chars"#);
        let frame = build_connect_frame(&jwt, "sig");
        let raw = frame.expose_secret();
        let json: serde_json::Value =
            serde_json::from_str(&raw["CONNECT ".len()..raw.len() - 2]).expect("still valid json");
        assert_eq!(json["jwt"], r#"evil"jwt\with{chars"#);
    }
}

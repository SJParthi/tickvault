//! Per-session ed25519 nkey keypair for the Groww native shadow client.
//!
//! Groww's NATS auth is decentralized nkey/JWT (verified from the growwapi
//! wheel — `feed.py::_key`): the client generates a FRESH ed25519 keypair per
//! session, sends the nkey-encoded PUBLIC key (`U…`) to the socket-token
//! endpoint (which returns a NATS user JWT bound to it), then signs the
//! server's `INFO` nonce with the PRIVATE key in the `CONNECT` frame.
//!
//! Key generation + signing use `aws-lc-rs` (workspace-pinned; the same crate
//! rustls already pulls — promoted to a direct dep in PR-R1, operator "go"
//! 2026-07-04). SECURITY: the private key lives ONLY inside the opaque
//! [`aws_lc_rs::signature::Ed25519KeyPair`]; this type derives NO `Debug`, the
//! seed is never exposed, never written to disk, never logged. A new session
//! (auth-failure re-mint) drops the old pair and generates a fresh one —
//! exactly the sidecar's rebuild semantics.

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair as _};

use crate::feed::groww::native::base64url;
use crate::feed::groww::nkey;

/// Why session-keypair construction failed. `Copy` — zero-alloc error path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeypairError {
    /// The crypto backend failed to generate a keypair (effectively
    /// unreachable on a healthy host — surfaced, never panicked).
    GenerationFailed,
    /// The generated public key did not nkey-encode (32-byte invariant
    /// violated — unreachable for a valid ed25519 key; surfaced defensively).
    EncodingFailed,
}

impl core::fmt::Display for KeypairError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            Self::GenerationFailed => "groww nkey: ed25519 keypair generation failed",
            Self::EncodingFailed => "groww nkey: public-key nkey encoding failed",
        };
        f.write_str(s)
    }
}

impl std::error::Error for KeypairError {}

/// A per-session ed25519 nkey keypair. NOT `Debug`/`Clone` — the private key
/// is opaque and single-owner by design.
pub struct GrowwSessionKeypair {
    key: Ed25519KeyPair,
    nkey_public: String,
}

impl GrowwSessionKeypair {
    /// Generate a FRESH per-session keypair (one per socket-token mint).
    /// The pure encode + sign surfaces are covered by the tests below, which
    /// also exercise this constructor end to end.
    // TEST-EXEMPT: thin composition over aws-lc-rs generate; exercised by every keypair test below.
    pub fn generate() -> Result<Self, KeypairError> {
        let key = Ed25519KeyPair::generate().map_err(|_| KeypairError::GenerationFailed)?;
        let nkey_public = nkey::encode_user_public_key(key.public_key().as_ref())
            .map_err(|_| KeypairError::EncodingFailed)?;
        Ok(Self { key, nkey_public })
    }

    /// The nkey-encoded PUBLIC key (`U…`) — the `socketKey` the token mint
    /// registers. Public material only; safe to send, never worth logging.
    #[must_use]
    pub fn nkey_public(&self) -> &str {
        &self.nkey_public
    }

    /// Sign the server `INFO` nonce and return the base64url (no-pad)
    /// signature string the `CONNECT` frame's `sig` field carries.
    #[must_use]
    pub fn sign_nonce_b64url(&self, nonce: &[u8]) -> String {
        let signature = self.key.sign(nonce);
        base64url::encode_no_pad(signature.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::signature::{ED25519, UnparsedPublicKey};

    /// The generated public key nkey-encodes to a `U…` string that round-trips
    /// through the merged nkey decoder back to the same 32 raw bytes.
    #[test]
    fn test_nkey_public_roundtrips_via_decoder() {
        let kp = GrowwSessionKeypair::generate().expect("keypair generates");
        let nkey_str = kp.nkey_public();
        assert!(nkey_str.starts_with('U'), "user nkey must start with U");
        let decoded = nkey::decode_user_public_key(nkey_str).expect("round-trips");
        assert_eq!(decoded.as_slice(), kp.key.public_key().as_ref());
    }

    /// Two sessions never share a keypair (fresh randomness per session).
    #[test]
    fn test_two_sessions_have_distinct_keys() {
        let a = GrowwSessionKeypair::generate().expect("a");
        let b = GrowwSessionKeypair::generate().expect("b");
        assert_ne!(a.nkey_public(), b.nkey_public());
    }

    /// The nonce signature verifies against the session public key and has the
    /// 86-char base64url (no-pad) length of a 64-byte ed25519 signature.
    #[test]
    fn test_sign_nonce_b64url_verifies() {
        let kp = GrowwSessionKeypair::generate().expect("keypair");
        let nonce = b"AXf29GJwrGY0Cq";
        let sig_b64 = kp.sign_nonce_b64url(nonce);
        assert_eq!(sig_b64.len(), 86);

        // Re-sign raw to verify with the crypto backend (the b64 path is
        // covered by the base64url vectors; ed25519 is deterministic, so the
        // raw signature is identical across calls).
        let raw = kp.key.sign(nonce);
        UnparsedPublicKey::new(&ED25519, kp.key.public_key().as_ref())
            .verify(nonce, raw.as_ref())
            .expect("signature must verify");
        assert_eq!(
            sig_b64,
            crate::feed::groww::native::base64url::encode_no_pad(raw.as_ref())
        );
    }
}

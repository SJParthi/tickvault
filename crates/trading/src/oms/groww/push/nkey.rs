//! NATS nkey codec + base64url + per-session ed25519 keypair for the Groww
//! order/position push channel — zero new deps beyond the workspace-pinned
//! `aws-lc-rs`. Restored byte-faithful (2026-07-16, order-push Stage B) from
//! the retired live-feed transport (`dd7eaa5e^:crates/core/src/feed/groww/
//! nkey.rs` + `native/base64url.rs` + `native/keypair.rs`, folded into one
//! file so the push module stays at its 7-file layout).
//!
//! Groww's NATS push channel uses **decentralized ed25519 (nkey) auth**: the
//! client generates an ed25519 key pair, registers its PUBLIC key with
//! Groww's socket-token endpoint (which returns a JWT bound to that key),
//! then signs the server's `INFO` nonce with the PRIVATE key in the
//! `CONNECT` frame.
//!
//! ## NATS nkey public-key wire format (from the nats.io nkeys spec)
//!
//! `base32_nopad( [prefix_byte] ++ public_key_32 ++ crc16_le(prefix++key) )`
//! where the USER public-key prefix byte is `20 << 3 = 160` → the encoded string
//! begins with `U`. `base32` is RFC 4648 (A–Z 2–7), **no padding**. `crc16` is the
//! XMODEM/CCITT variant (poly `0x1021`, init `0x0000`), appended little-endian.
//!
//! ## Guarantees (operator O(1) / worst-case)
//!
//! - Pure functions, no panic: encode is O(key length); decode bounds-checks
//!   every step and returns a typed `Copy` [`NkeyError`] (no heap on the error
//!   path) for bad length / bad base32 char / CRC mismatch / wrong prefix.
//! - Round-trip + known-answer tested (RFC 4648 base32 vectors, CRC16-XMODEM
//!   `0x31C3` for `"123456789"`), so a regression in either primitive fails the
//!   build before it can produce an auth-rejecting key string.

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair as _};

/// NATS USER public-key prefix byte (`20 << 3`). Encoded strings begin with `U`.
pub const PREFIX_BYTE_USER: u8 = 20 << 3;

/// RFC 4648 base32 alphabet (upper-case, no padding).
const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// An nkey encode/decode failure. `Copy` — zero-alloc error path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NkeyError {
    /// A public key was not exactly 32 bytes.
    BadKeyLength,
    /// The encoded string contained a non-base32 character.
    BadBase32Char,
    /// The decoded payload was too short to contain prefix + key + crc.
    TooShort,
    /// The trailing CRC16 did not match the payload.
    CrcMismatch,
    /// The leading prefix byte was not the expected one.
    WrongPrefix,
}

impl core::fmt::Display for NkeyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            Self::BadKeyLength => "nkey: public key must be 32 bytes",
            Self::BadBase32Char => "nkey: invalid base32 character",
            Self::TooShort => "nkey: decoded payload too short",
            Self::CrcMismatch => "nkey: CRC16 mismatch",
            Self::WrongPrefix => "nkey: unexpected prefix byte",
        };
        f.write_str(s)
    }
}

impl std::error::Error for NkeyError {}

/// CRC16 (XMODEM/CCITT: poly `0x1021`, init `0x0000`, no reflection) over `data`.
#[must_use]
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// RFC 4648 base32 encode (upper-case, NO padding). Cold path (key encoding).
#[must_use]
pub fn base32_encode_nopad(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for &byte in data {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let idx = ((buffer >> bits) & 0x1f) as usize;
            out.push(BASE32_ALPHABET[idx] as char);
        }
    }
    if bits > 0 {
        let idx = ((buffer << (5 - bits)) & 0x1f) as usize;
        out.push(BASE32_ALPHABET[idx] as char);
    }
    out
}

/// Map a base32 character to its 5-bit value, or `None` if not in the alphabet.
#[inline]
fn base32_value(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'2'..=b'7' => Some(c - b'2' + 26),
        _ => None,
    }
}

/// RFC 4648 base32 decode (upper-case, NO padding). Bounds-checked; rejects any
/// non-alphabet character with [`NkeyError::BadBase32Char`].
pub fn base32_decode_nopad(s: &str) -> Result<Vec<u8>, NkeyError> {
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for &c in s.as_bytes() {
        let v = base32_value(c).ok_or(NkeyError::BadBase32Char)?;
        buffer = (buffer << 5) | u32::from(v);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

/// Encode a raw 32-byte ed25519 public key as a NATS USER nkey string (`U…`).
///
/// `base32_nopad( [PREFIX_BYTE_USER] ++ key ++ crc16_le )`. This is the string the
/// Groww socket-token request registers so the returned JWT is bound to our key.
pub fn encode_user_public_key(public_key: &[u8]) -> Result<String, NkeyError> {
    if public_key.len() != 32 {
        return Err(NkeyError::BadKeyLength);
    }
    let mut raw = Vec::with_capacity(1 + 32 + 2);
    raw.push(PREFIX_BYTE_USER);
    raw.extend_from_slice(public_key);
    let crc = crc16(&raw);
    raw.extend_from_slice(&crc.to_le_bytes());
    Ok(base32_encode_nopad(&raw))
}

/// Decode a NATS USER nkey string back to the raw 32-byte public key, verifying
/// the prefix byte and the trailing CRC16. The inverse of
/// [`encode_user_public_key`] — used to round-trip-test the codec.
pub fn decode_user_public_key(encoded: &str) -> Result<[u8; 32], NkeyError> {
    let raw = base32_decode_nopad(encoded)?;
    // prefix(1) + key(32) + crc(2) = 35
    if raw.len() < 35 {
        return Err(NkeyError::TooShort);
    }
    if raw[0] != PREFIX_BYTE_USER {
        return Err(NkeyError::WrongPrefix);
    }
    let payload = &raw[..raw.len() - 2];
    let crc_bytes: [u8; 2] = raw[raw.len() - 2..]
        .try_into()
        .map_err(|_| NkeyError::TooShort)?;
    let stored = u16::from_le_bytes(crc_bytes);
    if crc16(payload) != stored {
        return Err(NkeyError::CrcMismatch);
    }
    let key: [u8; 32] = raw[1..33].try_into().map_err(|_| NkeyError::TooShort)?;
    Ok(key)
}

// ---------------------------------------------------------------------------
// URL-safe base64 (RFC 4648 §5, NO padding) — the encoding NATS requires for
// the `CONNECT` frame's `sig` field (base64url of the ed25519 nonce
// signature). Hand-written pure encoder, mirroring the hand-written base32
// above — zero new deps, fully vector-tested.
//
// Encode-only: the client never needs to DECODE base64url (the server never
// sends us one), so no decode surface is exposed.
// ---------------------------------------------------------------------------

/// RFC 4648 URL-safe alphabet (`+` → `-`, `/` → `_`).
const B64URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Encode `data` as URL-safe base64 WITHOUT padding.
///
/// Cold path (once per CONNECT handshake — a 64-byte signature); the single
/// pre-sized `String` allocation is intentional and outside any hot loop.
#[must_use]
pub fn encode_no_pad(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(B64URL_ALPHABET[(b0 >> 2) as usize] as char);
        out.push(B64URL_ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64URL_ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(B64URL_ALPHABET[(b2 & 0x3f) as usize] as char);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Per-session ed25519 nkey keypair.
//
// Groww's NATS auth is decentralized nkey/JWT (verified from the growwapi
// wheel — `feed.py::_key`): the client generates a FRESH ed25519 keypair per
// session, sends the nkey-encoded PUBLIC key (`U…`) to the socket-token
// endpoint (which returns a NATS user JWT bound to it), then signs the
// server's `INFO` nonce with the PRIVATE key in the `CONNECT` frame.
//
// Key generation + signing use `aws-lc-rs` (workspace-pinned; the same crate
// rustls already pulls). SECURITY: the private key lives ONLY inside the
// opaque `aws_lc_rs::signature::Ed25519KeyPair`; this type derives NO
// `Debug`, the seed is never exposed, never written to disk, never logged. A
// new session (auth-failure re-mint) drops the old pair and generates a
// fresh one.
// ---------------------------------------------------------------------------

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
        let nkey_public = encode_user_public_key(key.public_key().as_ref())
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
        encode_no_pad(signature.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::signature::{ED25519, UnparsedPublicKey};

    #[test]
    fn test_crc16_xmodem_known_answer() {
        // CRC16/XMODEM of "123456789" is 0x31C3 (the canonical KAT).
        assert_eq!(crc16(b"123456789"), 0x31C3);
        assert_eq!(crc16(b""), 0x0000);
    }

    #[test]
    fn test_base32_encode_nopad_rfc4648_vectors() {
        // RFC 4648 base32 test vectors (no padding).
        assert_eq!(base32_encode_nopad(b""), "");
        assert_eq!(base32_encode_nopad(b"f"), "MY");
        assert_eq!(base32_encode_nopad(b"fo"), "MZXQ");
        assert_eq!(base32_encode_nopad(b"foo"), "MZXW6");
        assert_eq!(base32_encode_nopad(b"foob"), "MZXW6YQ");
        assert_eq!(base32_encode_nopad(b"fooba"), "MZXW6YTB");
        assert_eq!(base32_encode_nopad(b"foobar"), "MZXW6YTBOI");
    }

    #[test]
    fn test_base32_decode_nopad_inverts_encode() {
        for v in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
            b"the quick brown fox",
        ] {
            let enc = base32_encode_nopad(v);
            assert_eq!(
                base32_decode_nopad(&enc).expect("decode"),
                v,
                "round-trip {v:?}"
            );
        }
    }

    #[test]
    fn test_base32_decode_rejects_bad_char() {
        // '0', '1', '8', '9', lowercase, '=' padding are not in the alphabet.
        assert_eq!(
            base32_decode_nopad("MZXW6YTB0I"),
            Err(NkeyError::BadBase32Char)
        );
        assert_eq!(
            base32_decode_nopad("mzxw6ytb"),
            Err(NkeyError::BadBase32Char)
        );
        assert_eq!(
            base32_decode_nopad("MY======"),
            Err(NkeyError::BadBase32Char)
        );
    }

    #[test]
    fn test_encode_user_public_key_starts_with_u_and_round_trips() {
        let key = [7u8; 32];
        let encoded = encode_user_public_key(&key).expect("encode");
        assert!(
            encoded.starts_with('U'),
            "USER nkey must start with U: {encoded}"
        );
        assert_eq!(decode_user_public_key(&encoded).expect("decode"), key);
    }

    #[test]
    fn test_encode_user_public_key_round_trips_varied_keys() {
        for seed in [0u8, 1, 42, 200, 255] {
            let mut key = [0u8; 32];
            for (i, b) in key.iter_mut().enumerate() {
                *b = seed.wrapping_add(i as u8);
            }
            let encoded = encode_user_public_key(&key).expect("encode");
            assert_eq!(decode_user_public_key(&encoded).expect("decode"), key);
        }
    }

    #[test]
    fn test_encode_rejects_wrong_key_length() {
        assert_eq!(
            encode_user_public_key(&[0u8; 31]),
            Err(NkeyError::BadKeyLength)
        );
        assert_eq!(
            encode_user_public_key(&[0u8; 33]),
            Err(NkeyError::BadKeyLength)
        );
        assert_eq!(encode_user_public_key(&[]), Err(NkeyError::BadKeyLength));
    }

    #[test]
    fn test_decode_detects_crc_corruption() {
        let key = [9u8; 32];
        let encoded = encode_user_public_key(&key).expect("encode");
        // flip the last char to a different valid base32 char → CRC must fail
        // (or, if it happened to change the key bytes cleanly, a wrong key — but
        // the CRC covers the whole payload so corruption is caught).
        let mut chars: Vec<char> = encoded.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == 'A' { 'B' } else { 'A' };
        let corrupted: String = chars.into_iter().collect();
        let result = decode_user_public_key(&corrupted);
        assert!(
            matches!(
                result,
                Err(NkeyError::CrcMismatch) | Err(NkeyError::TooShort)
            ),
            "corruption must be rejected, got {result:?}"
        );
    }

    #[test]
    fn test_decode_rejects_wrong_prefix() {
        // encode a 32-byte payload with a NON-user prefix byte and confirm reject.
        let mut raw = Vec::new();
        raw.push(0u8); // wrong prefix (account = 0)
        raw.extend_from_slice(&[5u8; 32]);
        let crc = crc16(&raw);
        raw.extend_from_slice(&crc.to_le_bytes());
        let encoded = base32_encode_nopad(&raw);
        assert_eq!(
            decode_user_public_key(&encoded),
            Err(NkeyError::WrongPrefix)
        );
    }

    #[test]
    fn test_decode_user_public_key_rejects_too_short() {
        assert_eq!(decode_user_public_key(""), Err(NkeyError::TooShort));
        assert_eq!(decode_user_public_key("MY"), Err(NkeyError::TooShort));
    }

    #[test]
    fn test_nkey_error_display_stable() {
        assert!(NkeyError::CrcMismatch.to_string().contains("CRC16"));
        assert!(NkeyError::BadKeyLength.to_string().contains("32 bytes"));
    }

    /// RFC 4648 §10 test vectors (padding stripped — we emit no-pad).
    #[test]
    fn test_encode_no_pad_rfc4648_vectors() {
        assert_eq!(encode_no_pad(b""), "");
        assert_eq!(encode_no_pad(b"f"), "Zg");
        assert_eq!(encode_no_pad(b"fo"), "Zm8");
        assert_eq!(encode_no_pad(b"foo"), "Zm9v");
        assert_eq!(encode_no_pad(b"foob"), "Zm9vYg");
        assert_eq!(encode_no_pad(b"fooba"), "Zm9vYmE");
        assert_eq!(encode_no_pad(b"foobar"), "Zm9vYmFy");
    }

    /// URL-safe alphabet: bytes that would produce `+`/`/` in standard base64
    /// must produce `-`/`_` here (0xfb 0xff → "-_8" in url-safe no-pad).
    #[test]
    fn test_encode_no_pad_uses_url_safe_alphabet() {
        let encoded = encode_no_pad(&[0xfb, 0xff]);
        assert_eq!(encoded, "-_8");
        assert!(!encoded.contains('+'));
        assert!(!encoded.contains('/'));
        assert!(!encoded.contains('='));
    }

    /// A 64-byte ed25519 signature encodes to exactly 86 chars (no padding).
    #[test]
    fn test_encode_no_pad_signature_length() {
        let sig = [0xA5u8; 64];
        assert_eq!(encode_no_pad(&sig).len(), 86);
    }

    /// The generated public key nkey-encodes to a `U…` string that round-trips
    /// through the nkey decoder back to the same 32 raw bytes.
    #[test]
    fn test_generate_nkey_public_roundtrips_via_decoder() {
        let kp = GrowwSessionKeypair::generate().expect("keypair generates");
        let nkey_str = kp.nkey_public();
        assert!(nkey_str.starts_with('U'), "user nkey must start with U");
        let decoded = decode_user_public_key(nkey_str).expect("round-trips");
        assert_eq!(decoded.as_slice(), kp.key.public_key().as_ref());
    }

    /// Two sessions never share a keypair (fresh randomness per session).
    #[test]
    fn test_generate_two_sessions_have_distinct_keys() {
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
        assert_eq!(sig_b64, encode_no_pad(raw.as_ref()));
    }
}

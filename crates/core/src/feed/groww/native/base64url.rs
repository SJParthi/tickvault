//! URL-safe base64 (RFC 4648 §5, NO padding) — the encoding NATS requires for
//! the `CONNECT` frame's `sig` field (base64url of the ed25519 nonce
//! signature). Hand-written pure encoder, mirroring the hand-written base32 in
//! [`crate::feed::groww::nkey`] — zero new deps, fully vector-tested.
//!
//! Encode-only: the client never needs to DECODE base64url (the server never
//! sends us one), so no decode surface is exposed.

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

#[cfg(test)]
mod tests {
    use super::*;

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
}

//! NATS nkey codec for the Groww live feed (operator 2026-06-19 "implement
//! everything"; §32 native-Rust path) — zero new deps, pure + fully testable.
//!
//! Groww's NATS feed uses **decentralized ed25519 (nkey) auth**: the client
//! generates an ed25519 key pair, registers its PUBLIC key with Groww's
//! socket-token endpoint (which returns a JWT bound to that key), then signs the
//! server's `INFO` nonce with the PRIVATE key in the `CONNECT` frame.
//!
//! This module is the **encoding** half — the human/wire text form of an nkey
//! public key (`U…`) that the socket-token request needs, built from the raw
//! 32-byte ed25519 public key. The ed25519 key generation + nonce SIGNING (via
//! `aws-lc-rs`, already in the dep tree) land in the signing slice; the live
//! `CONNECT` handshake in the connector slice.
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

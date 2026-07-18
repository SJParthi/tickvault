//! FNV-1a 64-bit signature hash — BIT-EXACT port of server.py's
//! `_signature_hash` (which itself mirrors the Rust summary_writer).
//! Feed order: `code`, `"|"`, `target`, `"|"`, first-160-CHARS of message
//! (UTF-8 bytes of each part).

use crate::pycompat::py_slice_chars;

const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

/// 16 lowercase hex chars, identical to the Python + summary_writer hash.
pub fn signature_hash(code: Option<&str>, target: &str, message: &str) -> String {
    let truncated = py_slice_chars(message, 160);
    let mut h = FNV_OFFSET;
    let parts: [&[u8]; 5] = [
        code.unwrap_or("").as_bytes(),
        b"|",
        target.as_bytes(),
        b"|",
        truncated.as_bytes(),
    ];
    for part in parts {
        for &b in part {
            h ^= u64::from(b);
            h = h.wrapping_mul(FNV_PRIME);
        }
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden vectors generated from the LIVE server.py on 2026-07-18:
    /// `python3 -c "import server; server._signature_hash(...)"` — see the
    /// PR 2c parity evidence. Bit-exactness is the load-bearing contract
    /// (list_novel_signatures / signature_history join on this hash).
    #[test]
    fn matches_python_golden_vectors() {
        let e200 = "é".repeat(200);
        let x300 = "x".repeat(300);
        let cases: &[(Option<&str>, &str, &str, &str)] = &[
            (None, "", "", "08e34c07b581a8a5"),
            (
                Some("DH-904"),
                "tickvault_core::oms",
                "rate limited",
                "500cf7253d9e7649",
            ),
            (Some(""), "a", "b", "d75e0331b7c7351c"),
            (
                Some("WS-GAP-05"),
                "tickvault_core::websocket::connection_pool",
                "pool slot respawned after unexpected clean exit attempt=3 backoff_secs=20",
                "879775901034f81d",
            ),
            (Some("I-P1-11"), "x", e200.as_str(), "8e628ae1a9607ccb"),
            (None, "target_only", "msg with | pipe", "e08b7bfb317cc8bc"),
            (Some("A"), "B", x300.as_str(), "1958b807b729b4be"),
        ];
        for (code, target, message, expected) in cases {
            assert_eq!(
                signature_hash(*code, target, message),
                *expected,
                "FNV-1a mismatch for code={code:?} target={target}"
            );
        }
    }

    #[test]
    fn none_code_equals_empty_code() {
        assert_eq!(
            signature_hash(None, "t", "m"),
            signature_hash(Some(""), "t", "m")
        );
    }

    #[test]
    fn truncation_is_160_chars_not_bytes() {
        // 200 two-byte chars: chars 160..200 must not affect the hash.
        let long = "é".repeat(200);
        let exact = "é".repeat(160);
        assert_eq!(
            signature_hash(Some("C"), "t", &long),
            signature_hash(Some("C"), "t", &exact)
        );
    }
}

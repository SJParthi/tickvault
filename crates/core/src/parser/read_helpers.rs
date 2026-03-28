//! Shared little-endian read helpers for binary packet parsing.
//!
//! These functions are used by all packet parsers (quote, full, market_depth, deep_depth).
//! Centralised here to eliminate duplication.
//!
//! # Safety Contract
//! Callers MUST validate buffer length before invoking. These functions index
//! directly into the slice for zero-overhead parsing on the hot path.

/// Reads a little-endian f32 from a byte slice at the given offset.
#[allow(clippy::arithmetic_side_effects)] // APPROVED: caller validates buffer length before invoking
#[inline(always)]
pub(super) fn read_f32_le(raw: &[u8], offset: usize) -> f32 {
    f32::from_le_bytes([
        raw[offset],
        raw[offset + 1],
        raw[offset + 2],
        raw[offset + 3],
    ])
}

/// Reads a little-endian u32 from a byte slice at the given offset.
#[allow(clippy::arithmetic_side_effects)] // APPROVED: caller validates buffer length before invoking
#[inline(always)]
pub(super) fn read_u32_le(raw: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        raw[offset],
        raw[offset + 1],
        raw[offset + 2],
        raw[offset + 3],
    ])
}

/// Reads a little-endian u16 from a byte slice at the given offset.
#[allow(clippy::arithmetic_side_effects)] // APPROVED: caller validates buffer length before invoking
#[inline(always)]
pub(super) fn read_u16_le(raw: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([raw[offset], raw[offset + 1]])
}

/// Reads a little-endian f64 from a byte slice at the given offset.
#[allow(clippy::arithmetic_side_effects)] // APPROVED: caller validates buffer length before invoking
#[inline(always)]
pub(super) fn read_f64_le(raw: &[u8], offset: usize) -> f64 {
    f64::from_le_bytes([
        raw[offset],
        raw[offset + 1],
        raw[offset + 2],
        raw[offset + 3],
        raw[offset + 4],
        raw[offset + 5],
        raw[offset + 6],
        raw[offset + 7],
    ])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- read_f32_le ---

    #[test]
    fn test_read_f32_le_zero() {
        let bytes = 0.0_f32.to_le_bytes();
        assert!((read_f32_le(&bytes, 0) - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_read_f32_le_positive() {
        let bytes = 21004.95_f32.to_le_bytes();
        assert!((read_f32_le(&bytes, 0) - 21004.95_f32).abs() < f32::EPSILON);
    }

    #[test]
    fn test_read_f32_le_negative() {
        let bytes = (-123.456_f32).to_le_bytes();
        assert!((read_f32_le(&bytes, 0) - -123.456_f32).abs() < f32::EPSILON);
    }

    #[test]
    fn test_read_f32_le_at_offset() {
        // 4 padding bytes, then the f32
        let mut buf = [0u8; 8];
        let value = 99.5_f32;
        buf[4..8].copy_from_slice(&value.to_le_bytes());
        assert!((read_f32_le(&buf, 4) - 99.5_f32).abs() < f32::EPSILON);
    }

    #[test]
    fn test_read_f32_le_at_nonzero_offset_with_junk_prefix() {
        let mut buf = [0xFF_u8; 12];
        let value = 42.0_f32;
        buf[8..12].copy_from_slice(&value.to_le_bytes());
        assert!((read_f32_le(&buf, 8) - 42.0_f32).abs() < f32::EPSILON);
    }

    #[test]
    fn test_read_f32_le_max_value() {
        let bytes = f32::MAX.to_le_bytes();
        assert!((read_f32_le(&bytes, 0) - f32::MAX).abs() < 1.0);
    }

    #[test]
    fn test_read_f32_le_min_positive() {
        let bytes = f32::MIN_POSITIVE.to_le_bytes();
        assert!((read_f32_le(&bytes, 0) - f32::MIN_POSITIVE).abs() < f32::EPSILON);
    }

    // --- read_u32_le ---

    #[test]
    fn test_read_u32_le_zero() {
        let bytes = 0_u32.to_le_bytes();
        assert_eq!(read_u32_le(&bytes, 0), 0);
    }

    #[test]
    fn test_read_u32_le_known_value() {
        let bytes = 1_000_000_u32.to_le_bytes();
        assert_eq!(read_u32_le(&bytes, 0), 1_000_000);
    }

    #[test]
    fn test_read_u32_le_max() {
        let bytes = u32::MAX.to_le_bytes();
        assert_eq!(read_u32_le(&bytes, 0), u32::MAX);
    }

    #[test]
    fn test_read_u32_le_at_offset() {
        let mut buf = [0u8; 12];
        let value = 12345_u32;
        buf[8..12].copy_from_slice(&value.to_le_bytes());
        assert_eq!(read_u32_le(&buf, 8), 12345);
    }

    #[test]
    fn test_read_u32_le_security_id() {
        // Typical Dhan security ID
        let bytes = 49081_u32.to_le_bytes();
        assert_eq!(read_u32_le(&bytes, 0), 49081);
    }

    #[test]
    fn test_read_u32_le_at_offset_with_junk() {
        let mut buf = [0xFF_u8; 8];
        let value = 256_u32;
        buf[4..8].copy_from_slice(&value.to_le_bytes());
        assert_eq!(read_u32_le(&buf, 4), 256);
    }

    // --- read_u16_le ---

    #[test]
    fn test_read_u16_le_zero() {
        let bytes = 0_u16.to_le_bytes();
        assert_eq!(read_u16_le(&bytes, 0), 0);
    }

    #[test]
    fn test_read_u16_le_known_value() {
        let bytes = 1234_u16.to_le_bytes();
        assert_eq!(read_u16_le(&bytes, 0), 1234);
    }

    #[test]
    fn test_read_u16_le_max() {
        let bytes = u16::MAX.to_le_bytes();
        assert_eq!(read_u16_le(&bytes, 0), u16::MAX);
    }

    #[test]
    fn test_read_u16_le_at_offset() {
        let mut buf = [0u8; 6];
        let value = 805_u16;
        buf[4..6].copy_from_slice(&value.to_le_bytes());
        assert_eq!(read_u16_le(&buf, 4), 805);
    }

    #[test]
    fn test_read_u16_le_message_length() {
        // Typical Dhan message length field
        let bytes = 162_u16.to_le_bytes();
        assert_eq!(read_u16_le(&bytes, 0), 162);
    }

    #[test]
    fn test_read_u16_le_at_offset_with_junk() {
        let mut buf = [0xFF_u8; 4];
        let value = 50_u16;
        buf[2..4].copy_from_slice(&value.to_le_bytes());
        assert_eq!(read_u16_le(&buf, 2), 50);
    }

    // --- read_f64_le ---

    #[test]
    fn test_read_f64_le_zero() {
        let bytes = 0.0_f64.to_le_bytes();
        assert!((read_f64_le(&bytes, 0) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_read_f64_le_positive() {
        let bytes = 21004.95_f64.to_le_bytes();
        assert!((read_f64_le(&bytes, 0) - 21004.95).abs() < f64::EPSILON);
    }

    #[test]
    fn test_read_f64_le_negative() {
        let bytes = (-9999.99_f64).to_le_bytes();
        assert!((read_f64_le(&bytes, 0) - -9999.99).abs() < f64::EPSILON);
    }

    #[test]
    fn test_read_f64_le_at_offset() {
        // 12 padding bytes, then the f64
        let mut buf = [0u8; 20];
        let value = 12345.6789_f64;
        buf[12..20].copy_from_slice(&value.to_le_bytes());
        assert!((read_f64_le(&buf, 12) - 12345.6789).abs() < f64::EPSILON);
    }

    #[test]
    fn test_read_f64_le_at_nonzero_offset_with_junk() {
        let mut buf = [0xFF_u8; 16];
        let value = 0.001_f64;
        buf[8..16].copy_from_slice(&value.to_le_bytes());
        assert!((read_f64_le(&buf, 8) - 0.001).abs() < f64::EPSILON);
    }

    #[test]
    fn test_read_f64_le_depth_price() {
        // Full Market Depth uses f64 prices (not f32)
        let bytes = 25650.50_f64.to_le_bytes();
        assert!((read_f64_le(&bytes, 0) - 25650.50).abs() < f64::EPSILON);
    }

    // --- Cross-function: verify LE byte order matters ---

    #[test]
    fn test_le_byte_order_u32() {
        // 0x01020304 in LE is [0x04, 0x03, 0x02, 0x01]
        let buf = [0x04, 0x03, 0x02, 0x01];
        assert_eq!(read_u32_le(&buf, 0), 0x01020304);
    }

    #[test]
    fn test_le_byte_order_u16() {
        // 0x0102 in LE is [0x02, 0x01]
        let buf = [0x02, 0x01];
        assert_eq!(read_u16_le(&buf, 0), 0x0102);
    }

    // --- Multiple reads from same buffer at different offsets ---

    #[test]
    fn test_multiple_f32_reads_from_same_buffer() {
        let v1 = 100.5_f32;
        let v2 = 200.75_f32;
        let v3 = 300.25_f32;
        let mut buf = [0u8; 12];
        buf[0..4].copy_from_slice(&v1.to_le_bytes());
        buf[4..8].copy_from_slice(&v2.to_le_bytes());
        buf[8..12].copy_from_slice(&v3.to_le_bytes());

        assert!((read_f32_le(&buf, 0) - v1).abs() < f32::EPSILON);
        assert!((read_f32_le(&buf, 4) - v2).abs() < f32::EPSILON);
        assert!((read_f32_le(&buf, 8) - v3).abs() < f32::EPSILON);
    }

    #[test]
    fn test_mixed_type_reads_from_same_buffer() {
        // Simulates a mini packet: u16 at 0, u32 at 2, f32 at 6
        let msg_len = 162_u16;
        let sec_id = 11536_u32;
        let ltp = 2100.50_f32;
        let mut buf = [0u8; 10];
        buf[0..2].copy_from_slice(&msg_len.to_le_bytes());
        buf[2..6].copy_from_slice(&sec_id.to_le_bytes());
        buf[6..10].copy_from_slice(&ltp.to_le_bytes());

        assert_eq!(read_u16_le(&buf, 0), 162);
        assert_eq!(read_u32_le(&buf, 2), 11536);
        assert!((read_f32_le(&buf, 6) - 2100.50_f32).abs() < f32::EPSILON);
    }

    // --- NaN and special float values ---

    #[test]
    fn test_read_f32_le_nan() {
        let bytes = f32::NAN.to_le_bytes();
        assert!(read_f32_le(&bytes, 0).is_nan());
    }

    #[test]
    fn test_read_f32_le_infinity() {
        let bytes = f32::INFINITY.to_le_bytes();
        assert!(read_f32_le(&bytes, 0).is_infinite());
        assert!(read_f32_le(&bytes, 0).is_sign_positive());
    }

    #[test]
    fn test_read_f32_le_neg_infinity() {
        let bytes = f32::NEG_INFINITY.to_le_bytes();
        assert!(read_f32_le(&bytes, 0).is_infinite());
        assert!(read_f32_le(&bytes, 0).is_sign_negative());
    }

    #[test]
    fn test_read_f32_le_negative_zero() {
        let bytes = (-0.0_f32).to_le_bytes();
        assert_eq!(read_f32_le(&bytes, 0), 0.0);
    }

    #[test]
    fn test_read_f64_le_nan() {
        let bytes = f64::NAN.to_le_bytes();
        assert!(read_f64_le(&bytes, 0).is_nan());
    }

    #[test]
    fn test_read_f64_le_infinity() {
        let bytes = f64::INFINITY.to_le_bytes();
        assert!(read_f64_le(&bytes, 0).is_infinite());
    }

    #[test]
    fn test_read_f64_le_neg_infinity() {
        let bytes = f64::NEG_INFINITY.to_le_bytes();
        assert!(read_f64_le(&bytes, 0).is_infinite());
        assert!(read_f64_le(&bytes, 0).is_sign_negative());
    }

    #[test]
    fn test_read_f64_le_max_value() {
        let bytes = f64::MAX.to_le_bytes();
        assert_eq!(read_f64_le(&bytes, 0), f64::MAX);
    }

    #[test]
    fn test_read_f64_le_min_positive() {
        let bytes = f64::MIN_POSITIVE.to_le_bytes();
        assert_eq!(read_f64_le(&bytes, 0), f64::MIN_POSITIVE);
    }

    // --- u32 boundary values ---

    #[test]
    fn test_read_u32_le_one() {
        let bytes = 1_u32.to_le_bytes();
        assert_eq!(read_u32_le(&bytes, 0), 1);
    }

    #[test]
    fn test_read_u32_le_large_value() {
        // Typical Dhan volume: 100 million
        let bytes = 100_000_000_u32.to_le_bytes();
        assert_eq!(read_u32_le(&bytes, 0), 100_000_000);
    }

    // --- u16 boundary values ---

    #[test]
    fn test_read_u16_le_one() {
        let bytes = 1_u16.to_le_bytes();
        assert_eq!(read_u16_le(&bytes, 0), 1);
    }

    #[test]
    fn test_read_u16_le_disconnect_code() {
        // Typical Dhan disconnect code
        let bytes = 805_u16.to_le_bytes();
        assert_eq!(read_u16_le(&bytes, 0), 805);
    }

    #[test]
    fn test_read_u16_le_all_ones() {
        let buf = [0xFF, 0xFF];
        assert_eq!(read_u16_le(&buf, 0), u16::MAX);
    }
}

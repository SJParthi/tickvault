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

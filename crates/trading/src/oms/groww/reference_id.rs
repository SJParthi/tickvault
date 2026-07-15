//! Groww `order_reference_id` generation + validation (ORD-PR-2 pure core).
//!
//! # The doc contract (verbatim, `16` §2 note)
//! "User provided 8 to 20 length alphanumeric string with atmost two
//! hypens (-)." [sic] — 8–20 chars, alphanumeric, at most two `-`.
//!
//! # Our format (design §4.4)
//! `TV` + `yyMMdd` (IST date) + 4-char base36 sequence + 4-char base36
//! random = **16 chars, uppercase alphanumeric, zero hyphens** — strictly
//! inside the doc contract, forever-unique by construction (uniqueness SCOPE
//! is Unknown O-10 ⇒ fail-closed never-recycle).
//!
//! - **Deterministic per intent:** the generator is a PURE function of
//!   `(ist date, sequence, random salt)` — the caller (executor, ORD-PR-3)
//!   fixes all three when the intent is recorded, so a replay of the SAME
//!   intent reuses the SAME reference id (the GA007 idempotency loop).
//! - **Collision-safe:** the sequence is `max observed in today's ledger
//!   replay + 1` (crash-safe monotone); the random suffix defends
//!   cross-process collision on the shared account.
//!
//! Everything here is pure — no clock, no RNG, no I/O (the caller supplies
//! date/seq/salt).

/// The fixed prefix marking a reference id as OURS (reconcile uses it to
/// separate our orders from BruteX co-tenant orders on the shared account).
pub const REFERENCE_ID_PREFIX: &str = "TV";

/// Total generated length: `TV`(2) + `yyMMdd`(6) + seq(4) + random(4).
pub const REFERENCE_ID_LEN: usize = 16;

/// Base36 alphabet size.
const BASE: u32 = 36;
/// 36^4 — the value space of each 4-char base36 block.
const BLOCK_SPACE: u32 = BASE * BASE * BASE * BASE; // 1_679_616

/// Maximum sequence value representable in the 4-char base36 block.
pub const MAX_SEQUENCE: u32 = BLOCK_SPACE - 1;

/// Doc-contract validator: 8–20 chars, every char ASCII alphanumeric or `-`,
/// at most two `-` (`16` §2 note verbatim). Pure, total, no-panic.
#[must_use]
pub fn is_valid_reference_id(s: &str) -> bool {
    let len = s.chars().count();
    if !(8..=20).contains(&len) {
        return false;
    }
    let mut hyphens = 0_u32;
    for c in s.chars() {
        if c == '-' {
            hyphens += 1;
            if hyphens > 2 {
                return false;
            }
        } else if !c.is_ascii_alphanumeric() {
            return false;
        }
    }
    true
}

/// Whether a (broker-echoed) reference id is OURS: our `TV` prefix + the
/// exact 16-char generated shape with a parseable date block. Foreign
/// (BruteX co-tenant) ids fail this and are counted + IGNORED by reconcile —
/// never touched (design §4.9).
#[must_use]
pub fn is_ours(s: &str) -> bool {
    decompose(s).is_some()
}

/// An IST calendar date for the reference-id date block. Pure data — the
/// caller derives it from its own IST clock (no clock read here).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IstDate {
    /// Full year (e.g. 2026).
    pub year: u16,
    /// Month 1–12.
    pub month: u8,
    /// Day 1–31.
    pub day: u8,
}

impl IstDate {
    /// `yyMMdd` — the 6-digit date block. Out-of-range month/day are clamped
    /// into 2-digit space by construction (`% 100`); the generator's own
    /// validity does not depend on calendar correctness (the block is an
    /// opaque namespace segment), but callers should pass a real date.
    #[must_use]
    pub fn yymmdd(self) -> String {
        format!(
            "{:02}{:02}{:02}",
            self.year % 100,
            u16::from(self.month) % 100,
            u16::from(self.day) % 100
        )
    }

    /// The 6-digit date block as a NUMBER (`yy*10_000 + mm*100 + dd`) — the
    /// same clamping as [`IstDate::yymmdd`]; the comparison form the
    /// cross-close classification consumes (`ReferenceIdParts::yymmdd`
    /// decodes to the same space).
    #[must_use]
    pub fn yymmdd_num(self) -> u32 {
        u32::from(self.year % 100) * 10_000
            + u32::from(self.month % 100) * 100
            + u32::from(self.day % 100)
    }
}

/// Encode `v % 36^4` as exactly 4 uppercase base36 chars (0-padded).
fn base36_block(v: u32) -> String {
    const DIGITS: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut v = v % BLOCK_SPACE;
    let mut out = [b'0'; 4];
    for slot in out.iter_mut().rev() {
        *slot = DIGITS[(v % BASE) as usize];
        v /= BASE;
    }
    // Construction guarantees ASCII.
    String::from_utf8_lossy(&out).into_owned()
}

/// Decode a 4-char uppercase base36 block. `None` on any non-base36 char.
fn parse_base36_block(s: &str) -> Option<u32> {
    if s.len() != 4 {
        return None;
    }
    let mut v: u32 = 0;
    for c in s.chars() {
        let d = match c {
            '0'..='9' => c as u32 - '0' as u32,
            'A'..='Z' => c as u32 - 'A' as u32 + 10,
            _ => return None,
        };
        v = v * BASE + d;
    }
    Some(v)
}

/// Generate a reference id — PURE function of `(date, sequence, random_salt)`.
///
/// Deterministic per intent: identical inputs ⇒ identical id (the GA007
/// replay contract). `sequence` and `random_salt` are folded into 4-char
/// base36 blocks (`% 36^4`); callers keep `sequence ≤` [`MAX_SEQUENCE`] for
/// bijectivity (a trading day is bounded far below 1.68M mutations by the
/// self-caps).
///
/// `random_salt` MUST come from a CSPRNG (`OsRng`) at the call site — a
/// predictable salt collapses the cross-process collision defense on the
/// shared account; ORD-PR-3 adds the ratchet pinning the `OsRng` call site.
///
/// The output ALWAYS satisfies [`is_valid_reference_id`] and [`is_ours`]:
/// 16 uppercase alphanumerics, zero hyphens.
#[must_use]
pub fn generate_reference_id(date: IstDate, sequence: u32, random_salt: u32) -> String {
    format!(
        "{REFERENCE_ID_PREFIX}{}{}{}",
        date.yymmdd(),
        base36_block(sequence),
        base36_block(random_salt)
    )
}

/// The decomposed parts of one of OUR reference ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferenceIdParts {
    /// The 6-digit `yyMMdd` date block, as digits (yy*10000 + mm*100 + dd).
    pub yymmdd: u32,
    /// The sequence block value.
    pub sequence: u32,
    /// The random-salt block value.
    pub random_salt: u32,
}

/// Decompose a reference id into its parts — `Some` ONLY for ids matching
/// our exact generated shape (prefix + 6 digits + 2 base36 blocks). Used by
/// ledger replay to recover today's max sequence (crash-safe monotone).
#[must_use]
pub fn decompose(s: &str) -> Option<ReferenceIdParts> {
    if s.len() != REFERENCE_ID_LEN || !s.starts_with(REFERENCE_ID_PREFIX) {
        return None;
    }
    let date_part = &s[2..8];
    if !date_part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let yymmdd: u32 = date_part.parse().ok()?;
    let sequence = parse_base36_block(&s[8..12])?;
    let random_salt = parse_base36_block(&s[12..16])?;
    Some(ReferenceIdParts {
        yymmdd,
        sequence,
        random_salt,
    })
}

/// Extract the sequence block from one of OUR reference ids (`None` for
/// foreign/malformed ids). Ledger replay folds this with `max()` to resume
/// the day's sequence after a crash.
#[must_use]
pub fn extract_sequence(s: &str) -> Option<u32> {
    decompose(s).map(|p| p.sequence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const DATE: IstDate = IstDate {
        year: 2026,
        month: 7,
        day: 15,
    };

    #[test]
    fn test_generate_shape_and_contract() {
        let id = generate_reference_id(DATE, 1, 0x2A);
        assert_eq!(id.len(), REFERENCE_ID_LEN);
        assert!(id.starts_with("TV260715"));
        assert!(is_valid_reference_id(&id));
        assert!(is_ours(&id));
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(id.chars().all(|c| !c.is_ascii_lowercase()));
    }

    #[test]
    fn test_generate_is_deterministic_per_intent() {
        let a = generate_reference_id(DATE, 42, 999);
        let b = generate_reference_id(DATE, 42, 999);
        assert_eq!(a, b);
        // Any input change changes the id (collision safety across intents).
        assert_ne!(a, generate_reference_id(DATE, 43, 999));
        assert_ne!(a, generate_reference_id(DATE, 42, 998));
        assert_ne!(
            a,
            generate_reference_id(
                IstDate {
                    year: 2026,
                    month: 7,
                    day: 16
                },
                42,
                999
            )
        );
    }

    #[test]
    fn test_decompose_roundtrip() {
        let id = generate_reference_id(DATE, 12_345, 67_890);
        let parts = decompose(&id).unwrap();
        assert_eq!(parts.yymmdd, 260_715);
        assert_eq!(parts.sequence, 12_345);
        assert_eq!(parts.random_salt, 67_890);
        assert_eq!(extract_sequence(&id), Some(12_345));
    }

    #[test]
    fn test_yymmdd_num_matches_string_block_and_decompose_space() {
        assert_eq!(DATE.yymmdd_num(), 260_715);
        assert_eq!(DATE.yymmdd(), "260715");
        // The decomposed date block of a generated id lands in the SAME
        // numeric space (the cross-close comparison contract).
        let id = generate_reference_id(DATE, 1, 1);
        assert_eq!(decompose(&id).unwrap().yymmdd, DATE.yymmdd_num());
        // Clamping matches the string form.
        let weird = IstDate {
            year: 2126,
            month: 107,
            day: 131,
        };
        assert_eq!(weird.yymmdd_num(), 26 * 10_000 + 7 * 100 + 31);
    }

    #[test]
    fn test_validator_doc_contract_boundaries() {
        // Length bounds: 8..=20.
        assert!(!is_valid_reference_id("A234567")); // 7
        assert!(is_valid_reference_id("A2345678")); // 8
        assert!(is_valid_reference_id(&"A".repeat(20))); // 20
        assert!(!is_valid_reference_id(&"A".repeat(21))); // 21
        assert!(!is_valid_reference_id(""));
        // Hyphens: at most two.
        assert!(is_valid_reference_id("AB-CD-1234"));
        assert!(!is_valid_reference_id("AB-CD-12-34"));
        // Non-alphanumeric refused.
        assert!(!is_valid_reference_id("ABCD_1234"));
        assert!(!is_valid_reference_id("ABCD 1234"));
        assert!(!is_valid_reference_id("ABCDé1234"));
    }

    #[test]
    fn test_is_ours_rejects_foreign_shapes() {
        assert!(!is_ours("BRUTEX-0001-XYZ")); // co-tenant style
        assert!(!is_ours("TV12345")); // too short
        assert!(!is_ours("tv260715000100AA")); // lowercase prefix
        assert!(!is_ours("TVAB0715000100AA")); // non-digit date block
        assert!(!is_ours("TV2607150001-0AA")); // hyphen in block
        assert!(!is_ours(&format!("{}X", generate_reference_id(DATE, 1, 1)))); // 17 chars
        assert_eq!(extract_sequence("BRUTEX-0001-XYZ"), None);
    }

    #[test]
    fn test_base36_block_padding_and_wraparound() {
        assert_eq!(base36_block(0), "0000");
        assert_eq!(base36_block(35), "000Z");
        assert_eq!(base36_block(36), "0010");
        assert_eq!(base36_block(MAX_SEQUENCE), "ZZZZ");
        // Wraparound is modulo, never a panic.
        assert_eq!(base36_block(BLOCK_SPACE), "0000");
        assert_eq!(parse_base36_block("ZZZZ"), Some(MAX_SEQUENCE));
        assert_eq!(parse_base36_block("zzzz"), None); // uppercase only
        assert_eq!(parse_base36_block("Z-ZZ"), None);
        assert_eq!(parse_base36_block("ZZZ"), None);
    }

    proptest! {
        /// The generated id ALWAYS satisfies the doc contract + our shape,
        /// for arbitrary dates (incl. garbage month/day) and block values.
        #[test]
        fn prop_reference_id_always_matches_contract(
            year in 0_u16..=9999,
            month in 0_u8..=255,
            day in 0_u8..=255,
            seq in 0_u32..=u32::MAX,
            salt in 0_u32..=u32::MAX,
        ) {
            let id = generate_reference_id(IstDate { year, month, day }, seq, salt);
            prop_assert_eq!(id.len(), REFERENCE_ID_LEN);
            prop_assert!(is_valid_reference_id(&id));
            prop_assert!(is_ours(&id));
            prop_assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
            // Sequence survives the roundtrip modulo the block space.
            prop_assert_eq!(extract_sequence(&id), Some(seq % BLOCK_SPACE));
        }

        /// The validator never panics on arbitrary unicode input.
        #[test]
        fn prop_validator_total_no_panic(s in "\\PC{0,64}") {
            let _ = is_valid_reference_id(&s);
            let _ = is_ours(&s);
            let _ = extract_sequence(&s);
        }

        /// Determinism: the same inputs always produce the same id.
        #[test]
        fn prop_generator_deterministic(seq in 0_u32..BLOCK_SPACE, salt in 0_u32..BLOCK_SPACE) {
            let a = generate_reference_id(DATE, seq, salt);
            let b = generate_reference_id(DATE, seq, salt);
            prop_assert_eq!(a, b);
        }
    }
}

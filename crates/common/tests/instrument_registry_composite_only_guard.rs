//! I-P1-11 ratchet: the registry exposes ONLY segment-aware lookups.
//!
//! Why this guard exists (2026-08-08). `InstrumentRegistry` used to carry TWO
//! indexes: the authoritative `by_composite` keyed on the I-P1-11 composite
//! `(security_id, exchange_segment)`, and a LEGACY `instruments` map keyed on
//! `security_id` alone. Two public accessors read the legacy map — `get(id)`
//! and `contains(id)` — and their own doc comments admitted they returned
//! "whichever entry won the insert race" when Dhan reuses a numeric id across
//! segments (the live 2026-04-17 incident: FINNIFTY `IDX_I` id=27 alongside an
//! `NSE_EQ` id=27; NIFTY `IDX_I` id=13 alongside an `NSE_EQ` id=13, which made
//! index ticks silently vanish from the tick table).
//!
//! The legacy field's doc claimed it was "kept for the 59 existing call sites
//! that do not know the segment at lookup time". A full sweep on 2026-08-08
//! found ZERO production call sites — the 5 real ones were migrated to
//! `get_with_segment` on 2026-04-17 (commit c7397b3). What remained was a
//! second full copy of every instrument plus a footgun API, kept alive to serve
//! its own unit tests, one DHAT test, and one Criterion bench.
//!
//! All three were migrated and the lossy accessors deleted, so a
//! wrong-instrument read is now structurally impossible rather than merely
//! discouraged. This guard stops them coming back. It complements — not
//! replaces — `.claude/hooks/banned-pattern-scanner.sh` category 5, which
//! blocks NEW `.registry.get(id)` call sites: the hook guards call sites, this
//! guards the API surface itself.

#![cfg(test)]

use std::path::PathBuf;

fn registry_src() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/instrument_registry.rs")
}

/// Strip `//`-comment lines. REQUIRED: the deletion notes left in the source
/// deliberately quote the removed signatures (e.g. "`pub fn get(&self, ...)`
/// was DELETED here"), so a raw substring scan would match its own tombstone
/// and fail forever.
fn strip_comment_lines(src: &str) -> String {
    src.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn lossy_id_only_accessors_stay_deleted() {
    let src = std::fs::read_to_string(registry_src())
        .unwrap_or_else(|e| panic!("cannot read instrument_registry.rs: {e}"));
    let code = strip_comment_lines(&src);

    assert!(
        !code.contains("pub fn get("),
        "`pub fn get(security_id)` has been re-added to InstrumentRegistry.\n\n\
         That accessor reads a security_id-only index, so on a cross-segment id \
         reuse it returns the WRONG instrument — silently. It was deleted on \
         2026-08-08 after a sweep proved it had zero production callers.\n\
         Use `get_with_segment(id, segment)`; the segment is always available \
         (the tick processor reads it from byte 3 of the 8-byte binary header).\n\
         See .claude/rules/project/security-id-uniqueness.md (I-P1-11)."
    );

    assert!(
        !code.contains("pub fn contains("),
        "`pub fn contains(security_id)` has been re-added to InstrumentRegistry.\n\n\
         It reported `true` for an id that exists only under a DIFFERENT \
         segment — a false positive by construction. Use \
         `contains_with_segment(id, segment)`.\n\
         See .claude/rules/project/security-id-uniqueness.md (I-P1-11)."
    );

    assert!(
        !code.contains("instruments: HashMap<SecurityId"),
        "the LEGACY single-segment `instruments: HashMap<SecurityId, \
         SubscribedInstrument>` index has been re-added.\n\n\
         It stored a second full copy of every instrument and dropped one entry \
         of every cross-segment collision. `by_composite` — keyed on \
         `(security_id, exchange_segment)` — is the single source of truth.\n\
         See .claude/rules/project/security-id-uniqueness.md (I-P1-11)."
    );
}

#[test]
fn segment_aware_accessors_still_exist() {
    // Non-vacuity: the three assertions above would also pass if someone
    // deleted the whole file or renamed every accessor. Pin that the CORRECT
    // API is present, so this guard can never be satisfied by absence.
    let src = std::fs::read_to_string(registry_src())
        .unwrap_or_else(|e| panic!("cannot read instrument_registry.rs: {e}"));
    let code = strip_comment_lines(&src);

    assert!(
        code.contains("pub fn get_with_segment("),
        "`get_with_segment` is missing — the segment-aware lookup is the ONLY \
         sanctioned way to resolve an instrument (I-P1-11)."
    );
    assert!(
        code.contains("pub fn contains_with_segment("),
        "`contains_with_segment` is missing — see I-P1-11."
    );
    assert!(
        code.contains("by_composite"),
        "the composite `(security_id, exchange_segment)` index is missing — \
         this is the I-P1-11 source of truth."
    );
}

#[test]
fn collision_reporting_survived_the_deletion() {
    // The legacy map was ALSO how cross-segment collisions were detected (via a
    // duplicate-insert probe). Removing it must not have silently removed the
    // I-P1-11 operator signal: `cross_segment_collisions()` feeds the
    // `tv_instrument_registry_cross_segment_collisions` gauge surfaced by
    // /health and `make doctor`, and `collision_pairs` names WHICH ids clashed.
    let src = std::fs::read_to_string(registry_src())
        .unwrap_or_else(|e| panic!("cannot read instrument_registry.rs: {e}"));
    let code = strip_comment_lines(&src);

    assert!(
        code.contains("pub fn cross_segment_collisions("),
        "`cross_segment_collisions()` is gone — the I-P1-11 collision gauge \
         would go dark. Collision COUNTING must survive any refactor of the \
         registry's internal indexes."
    );
    assert!(
        code.contains("collision_pairs"),
        "`collision_pairs` is gone — operators would lose the list of WHICH \
         security_ids collided across segments."
    );
    assert!(
        code.contains("last_segment_for_id"),
        "the construction-time collision probe (`last_segment_for_id`) is gone. \
         Collision detection previously relied on a duplicate insert into the \
         deleted legacy map; if that probe is removed, \
         `cross_segment_collisions()` silently returns 0 forever — a false-OK \
         (audit-findings Rule 11)."
    );
}

#[test]
fn no_production_call_site_uses_an_id_only_registry_lookup() {
    // Workspace sweep. The methods are gone, so any such call would fail to
    // compile — but this asserts it at the SOURCE level too, giving a far
    // clearer message than a type error if someone re-adds the accessor and a
    // call site in the same change.
    let crates_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut offenders: Vec<String> = Vec::new();

    fn walk(dir: &PathBuf, offenders: &mut Vec<String>, depth: usize) {
        if depth > 12 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                walk(&path, offenders, depth.saturating_add(1));
            } else if path.extension().is_some_and(|e| e == "rs") {
                // Self-exemption: this guard must name the banned call shapes
                // in order to ban them, so it necessarily contains them as
                // string literals. Same convention as
                // `crates/common/tests/rust_only_guard.rs`, which is likewise
                // exempt from its own scan.
                if path
                    .file_name()
                    .is_some_and(|n| n == "instrument_registry_composite_only_guard.rs")
                {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                for (idx, line) in text.lines().enumerate() {
                    if line.trim_start().starts_with("//") {
                        continue;
                    }
                    if line.contains("registry.get(") || line.contains("registry.contains(") {
                        offenders.push(format!("{}:{}", path.display(), idx.saturating_add(1)));
                    }
                }
            }
        }
    }
    walk(&crates_dir, &mut offenders, 0);

    assert!(
        offenders.is_empty(),
        "found {} id-only registry lookup call site(s): {:?}\n\n\
         Use `get_with_segment(id, segment)` / `contains_with_segment(id, segment)`. \
         See .claude/rules/project/security-id-uniqueness.md (I-P1-11).",
        offenders.len(),
        offenders
    );
}

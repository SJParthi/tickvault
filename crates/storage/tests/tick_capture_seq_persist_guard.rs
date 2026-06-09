//! TICK-SEQ-01 PR-2b regression guard (adversarial review findings C1 + M1).
//!
//! The `ticks` DEDUP key is `(ts, security_id, segment, capture_seq)`. EVERY
//! production tick persist MUST thread the replay-stable `capture_seq` via
//! `append_tick_with_seq` / `append_tick_enriched_with_seq`. A seq-LESS
//! `append_tick(` / `append_tick_enriched(` call on a production path stamps a
//! fresh `next_capture_seq()` → on WAL replay the same frame is re-delivered
//! with its original `frame_seq`, so the live row and the replayed row get
//! DIFFERENT `capture_seq` → a DUPLICATE row under the new key.
//!
//! Review C1 was exactly such a missed call site (the Full-packet arm of
//! `tick_processor.rs`); the fast-boot double-writer (`run_tick_persistence_consumer`)
//! was a second instance. This guard fails the build if either regresses.

use std::path::Path;

/// Production source files that own a `TickPersistenceWriter` and persist ticks.
/// Their inline `#[cfg(test)]` modules are excluded so test-only seq-less calls
/// (legitimate — they exercise the delegate) do not trip the guard.
const PERSIST_SOURCES: &[&str] = &[
    "../core/src/pipeline/tick_processor.rs",
    "../app/src/main.rs",
];

/// Drop the inline test module so test-only seq-less calls are not scanned.
fn production_slice(src: &str) -> &str {
    match src.find("\n#[cfg(test)]\nmod tests") {
        Some(idx) => &src[..idx],
        None => src,
    }
}

#[test]
fn no_seqless_append_tick_on_production_persist_path() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut violations = Vec::new();

    for rel in PERSIST_SOURCES {
        let path = manifest.join(rel);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let prod = production_slice(&src);
        for (idx, line) in prod.lines().enumerate() {
            // Match the seq-LESS calls EXACTLY: the `(` immediately follows the
            // name, which distinguishes `append_tick(` from `append_tick_with_seq(`.
            if line.contains(".append_tick(") || line.contains(".append_tick_enriched(") {
                violations.push(format!("{}:{}: {}", rel, idx + 1, line.trim()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "TICK-SEQ-01 C1 regression: production tick persistence MUST use \
         `append_tick_with_seq` / `append_tick_enriched_with_seq` (replay-stable \
         capture_seq), NOT the seq-less variants — else WAL replay duplicates the \
         row under the (ts, security_id, segment, capture_seq) dedup key. \
         Offending production lines:\n{}",
        violations.join("\n")
    );
}

#[test]
fn guard_sources_exist() {
    // Defends the guard itself: if a scanned file is renamed/moved, fail loudly
    // rather than silently scanning nothing.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    for rel in PERSIST_SOURCES {
        let path = manifest.join(rel);
        assert!(
            path.exists(),
            "TICK-SEQ-01 persist guard source missing: {} — update PERSIST_SOURCES",
            path.display()
        );
    }
}

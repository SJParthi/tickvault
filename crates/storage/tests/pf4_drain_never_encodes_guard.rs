//! PF-4 ratchet (plan item PF-4, 2026-11-06): the frame drain's tick append
//! path never writes ILP text again.
//!
//! Since PF-4 the drain stages typed rows (`tick_stage::StagedBatch`) and the
//! writer thread, the rescue thread and the drain's cold blocking paths write
//! the text. A later edit that puts an ILP write back into an append function
//! would quietly undo the move and put ~300 bytes of digit formatting per tick
//! back on the task that empties the sockets. This scan fails the build when
//! any append-path body names an ILP writer, and pins the order the encoders
//! keep relative to the acknowledgement and the rescue floor (owner decision
//! 2026-11-06: a refused row is counted before its batch is acknowledged).

use std::path::PathBuf;

fn source(file: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The production half (before the first `#[cfg(test)]` module).
fn production(file: &str) -> String {
    let src = source(file);
    src.split("\n#[cfg(test)]\nmod ")
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// The body of the first function whose signature contains `sig`, up to the
/// matching closing brace.
fn body_of(src: &str, sig: &str) -> String {
    let start = src
        .find(sig)
        .unwrap_or_else(|| panic!("`{sig}` not found; the scan is stale"));
    let open = start + src[start..].find('{').expect("body opens");
    let mut depth = 0_i32;
    for (i, c) in src[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return src[start..open + i + 1].to_owned();
                }
            }
            _ => {}
        }
    }
    panic!("`{sig}` body never closes");
}

/// Names that write ILP text. None may appear on the drain's append path.
const ILP_WRITERS: [&str; 8] = [
    "write_tick_line",
    "append_aux_row",
    "IlpLine",
    "append_within_marker",
    "encode_into",
    "encode_counted",
    "encode_readonly",
    "TickRowBuffer",
];

#[test]
fn pf4_the_drain_append_path_writes_no_ilp_text() {
    let src = production("tick_persistence.rs");
    for sig in [
        "fn append_tick_into(",
        "pub(crate) fn append_row(",
        "fn append_quote_row(",
        "pub fn append_aux(",
        "fn stage_row(",
        "fn persist_refused_tick(",
        "fn note_pending_seq(",
    ] {
        let body = body_of(&src, sig);
        for writer in ILP_WRITERS {
            assert!(
                !body.contains(writer),
                "`{sig}` names `{writer}`: the drain must stage rows, never write ILP text (PF-4)"
            );
        }
    }
    for sig in [
        "fn append_row(",
        "fn append_quote_row(",
        "pub fn append_aux(",
    ] {
        assert!(
            body_of(&src, sig).contains("self.stage_row("),
            "`{sig}` must stage its row"
        );
    }
}

/// The encoders count a refused row BEFORE the batch is acknowledged (writer
/// thread) or its rescue floor released (rescue thread).
#[test]
fn pf4_a_refused_row_is_counted_before_the_ack_and_the_floor_release() {
    let src = production("tick_persistence.rs");
    let write = body_of(&src, "fn write_inner(&mut self, batch: &mut FlushBatch)");
    let encode = write
        .find("encode_into(")
        .expect("the writer thread encodes");
    let ack = write
        .find("note_ticks_acked(batch.max_seq)")
        .expect("the writer thread acks");
    let send = write
        .find("sender.flush(")
        .expect("the writer thread sends");
    assert!(encode < ack && encode < send, "encode before send and ack");
    let rescue = body_of(&src, "pub fn rescue(&self, batch: &RescueBatch)");
    let encode = rescue
        .find("encode_counted(")
        .expect("the rescue thread encodes");
    let release = rescue
        .find("release_rescue_floor(")
        .expect("the rescue thread releases the floor");
    assert!(encode < release, "encode before the floor release");
    let stage = production("tick_stage.rs");
    let backstop = body_of(&stage, "fn note_encode_failure(");
    let unapplied = backstop.find("note_unapplied(").expect("marks unapplied");
    let dropped = backstop
        .find("tv_ticks_dropped_total")
        .expect("counts the drop");
    let ledger = backstop
        .find("note_rows_not_landed(1)")
        .expect("ledger not-landed");
    assert!(
        unapplied < dropped && dropped < ledger,
        "the drain's old order"
    );
}

/// The test-only text helper has no production caller.
#[test]
fn pf4_encode_for_test_has_no_production_caller() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut offenders = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            let name = path.to_string_lossy().to_string();
            if !name.ends_with(".rs") || !name.contains("/src/") {
                continue;
            }
            let file = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if file.contains("test") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let prod = text
                .split("\n#[cfg(test)]\nmod ")
                .next()
                .unwrap_or_default();
            let calls = prod.matches(".encode_ilp_for_test(").count();
            if calls > 0 {
                offenders.push(name);
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "encode_ilp_for_test is tests-only (it allocates): {offenders:?}"
    );
}

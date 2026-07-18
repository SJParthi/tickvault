//! `--self-test` mode — the same print sequence as server.py's
//! `_self_test()` (server.py:1769-1804). JSON demo blocks are truncated
//! to the first 400 CHARACTERS like the Python `[:400]` slices.
//!
//! Documented deviation: JSON key ORDER inside the demo blocks differs
//! (serde_json sorts keys; Python preserves insertion order). The
//! self-test is a human smoke surface, not part of the byte-compared
//! parity transcript.

use std::io::Write;

use serde_json::Value;

use crate::config::{self, Ctx};
use crate::pycompat::py_slice_chars;
use crate::tools;

fn dumps_indent2(v: &Value) -> String {
    // Python's demo blocks are json.dumps(..., indent=2) with the
    // ensure_ascii=True default; the [:400] slice runs over the ESCAPED
    // string, so escape BEFORE the caller's py_slice_chars (review r8,
    // 2026-07-18).
    crate::pycompat::ensure_ascii(&serde_json::to_string_pretty(v).unwrap_or_default())
}

/// Python's novel-demo compaction: keep every top-level key, but replace
/// `novel` with its first 3 entries, each stripped of `message`.
fn compact_novel_demo(out: &Value) -> Value {
    let Some(map) = out.as_object() else {
        return out.clone();
    };
    let mut compact = serde_json::Map::new();
    for (k, v) in map {
        if k == "novel" {
            let trimmed: Vec<Value> = v
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .take(3)
                        .map(|ev| {
                            let mut obj = ev.as_object().cloned().unwrap_or_default();
                            obj.remove("message");
                            Value::Object(obj)
                        })
                        .collect()
                })
                .unwrap_or_default();
            compact.insert(k.clone(), Value::Array(trimmed));
        } else {
            compact.insert(k.clone(), v.clone());
        }
    }
    Value::Object(compact)
}

/// Run the self-test; returns the process exit code (always 0, like the
/// Python `_self_test`).
pub fn run(ctx: &Ctx) -> i32 {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut emit = |line: &str| {
        let _ignored = writeln!(out, "{line}");
    };

    emit("tickvault-logs MCP server self-test");
    emit(&format!("logs dir: {}", ctx.logs_dir().display()));
    emit(&format!(
        "state dir: {}",
        config::state_dir(&ctx.repo_root).display()
    ));
    emit("");
    let descs = tools::tool_descriptions();
    emit(&format!("tools registered: {}", descs.len()));
    for (name, desc) in &descs {
        emit(&format!("  - {name}: {}...", py_slice_chars(desc, 70)));
    }
    emit("");
    emit("--- tool_summary_snapshot() demo ---");
    emit(py_slice_chars(
        &dumps_indent2(&tools::tool_summary_snapshot(ctx)),
        400,
    ));
    emit("");
    emit("--- tool_triage_log_tail(limit=3) demo ---");
    emit(py_slice_chars(
        &dumps_indent2(&tools::tool_triage_log_tail(ctx, 3)),
        400,
    ));
    emit("");
    emit("--- tool_tail_errors(limit=3) demo ---");
    emit(py_slice_chars(
        &dumps_indent2(&tools::tool_tail_errors(ctx, 3, None)),
        400,
    ));
    emit("");
    emit("--- tool_list_novel_signatures(since_minutes=60) demo ---");
    // since_minutes=60 is deep inside the Python success band, so the
    // Err arm is unreachable here (Python's demo would crash if it ever
    // raised); degrade to a visible error object rather than panic.
    let novel_out = tools::tool_list_novel_signatures(ctx, 60)
        .unwrap_or_else(|e| serde_json::json!({"error": e}));
    emit(py_slice_chars(
        &dumps_indent2(&compact_novel_demo(&novel_out)),
        400,
    ));
    emit("");
    emit("self-test done");
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compact_novel_demo_strips_message_and_takes_first_3() {
        let out = json!({
            "dir": "/x",
            "since_minutes": 60,
            "cutoff_utc": "t",
            "novel_count": 4,
            "novel": [
                {"signature": "a", "message": "m1", "code": "C1"},
                {"signature": "b", "message": "m2"},
                {"signature": "c", "message": "m3"},
                {"signature": "d", "message": "m4"},
            ],
        });
        let compact = compact_novel_demo(&out);
        let novel = compact["novel"].as_array().unwrap();
        assert_eq!(novel.len(), 3);
        for ev in novel {
            assert!(ev.get("message").is_none());
        }
        assert_eq!(novel[0]["signature"], "a");
        assert_eq!(compact["novel_count"], 4);
    }
}

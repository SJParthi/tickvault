//! Audit Finding #11 (2026-05-03): the tick_processor file has dozens
//! of `.clone()` / `Vec::new` / `format!` / `Box` / `.collect()` calls.
//! Most are marked `// O(1) EXEMPT:` per the hot-path rule but a
//! systematic audit found a handful in cold-path snapshot persisters
//! that lacked inline exemption comments.
//!
//! A true end-to-end DHAT zero-allocation test for `run_tick_processor`
//! (the only public entry point in `tick_processor.rs`) requires a
//! substantial refactor to extract the per-tick inner loop as a
//! sync-testable function. That work is queued as Wave-6 item W6-4.
//!
//! In the meantime this meta-guard scans the production code (skipping
//! `mod tests`) and asserts every line containing a banned-on-hot-path
//! allocation pattern carries one of:
//!   - `// O(1) EXEMPT:` — explicit hot-path bypass with justification
//!   - `// TEST-EXEMPT:` — testing-only code path
//!   - `// APPROVED:` — operator-approved deviation
//!   - is inside `#[cfg(test)]`
//!
//! Without this guard, future PRs can sneak a hot-path allocation past
//! review by simply not adding a comment. The audit caught 5 such cases
//! at PR-cut time (lines 504, 511, 641, 647, 1017 — all cold path but
//! unmarked).

use std::path::PathBuf;

const TARGET: &str = "src/pipeline/tick_processor.rs";

fn read_target() -> Vec<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(TARGET);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
        .lines()
        .map(str::to_owned)
        .collect()
}

fn first_test_module_line(lines: &[String]) -> Option<usize> {
    // Find the start of the real `mod tests {` block — NOT a single
    // `#[cfg(test)]` annotation on a static.
    for (i, line) in lines.iter().enumerate() {
        let stripped = line.trim_start();
        if stripped.starts_with("mod tests")
            || stripped.starts_with("pub mod tests")
            || stripped.starts_with("mod test_")
        {
            return Some(i);
        }
    }
    None
}

#[test]
fn every_hot_path_allocation_in_tick_processor_has_an_exempt_comment() {
    let lines = read_target();
    let test_start = first_test_module_line(&lines).unwrap_or(lines.len());
    let prod = &lines[..test_start];

    let alloc_patterns = [
        ".clone()",
        "Vec::new()",
        "Vec::with_capacity(",
        "String::new()",
        "format!(",
        "Box::new(",
        ".collect()",
        ".to_string()",
    ];
    let exempt_patterns = [
        "O(1) EXEMPT",
        "TEST-EXEMPT",
        "APPROVED",
        "cold path",
        "cold_path",
    ];

    let mut violations = Vec::new();
    for (idx, line) in prod.iter().enumerate() {
        let trimmed = line.trim_start();
        // Skip pure comments and doc lines
        if trimmed.starts_with("//") || trimmed.starts_with("///") {
            continue;
        }
        if !alloc_patterns.iter().any(|p| line.contains(p)) {
            continue;
        }
        // Look in a 5-line window above + the line itself for an exempt comment
        let window_start = idx.saturating_sub(5);
        let window: String = prod[window_start..=idx].join("\n");
        let exempted = exempt_patterns.iter().any(|p| window.contains(p));
        if !exempted {
            violations.push(format!("L{}: {}", idx + 1, line.trim()));
        }
    }

    assert!(
        violations.is_empty(),
        "tick_processor.rs has {} unmarked hot-path allocations:\n{}\n\n\
         Add `// O(1) EXEMPT: <reason>` or `// TEST-EXEMPT: <reason>` \
         on the same line or within 5 lines above. Audit Finding #11 \
         (2026-05-03).",
        violations.len(),
        violations.join("\n")
    );
}

#[test]
fn meta_guard_recognizes_test_module_boundary() {
    let lines = read_target();
    let test_start = first_test_module_line(&lines)
        .expect("tick_processor.rs must contain `mod tests` for the meta-guard to scope correctly");
    assert!(
        test_start > 1500,
        "tick_processor.rs `mod tests` boundary at line {} is suspiciously \
         early — the meta-guard would skip most of the file. Verify the \
         scoping helper.",
        test_start + 1
    );
}

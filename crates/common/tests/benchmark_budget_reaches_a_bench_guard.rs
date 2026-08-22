//! A latency budget that matches no benchmark enforces nothing.
//!
//! `quality/benchmark-budgets.toml` is described in `CLAUDE.md` as *"the
//! executable source of truth"* for the O(1) latency claim. `bench-gate.sh`
//! reads it, normalises each Criterion id (strip `/new`, `/` → `_`, lowercase)
//! and matches a budget by **bidirectional substring** — either string
//! containing the other counts (`bench-gate.sh:275-294`).
//!
//! Nothing checked the other direction: a budget key matching *no* bench is
//! read, parsed, and then never compared against anything. It sits in the file
//! looking like an enforced ceiling and gates nothing at all.
//!
//! **This has already happened here.** The comment beside the `moneyness` key
//! records `should_emit_snapshot` being *"deleted 2026-07-17 as an orphaned
//! budget"*, and goes on to say the new key was *"checked bidirectionally
//! against every other budget key and bench name"* — by hand, in a comment.
//! That is the right check performed the wrong way: it protects the one key
//! whose author remembered to do it, once.
//!
//! The bidirectional rule is also why this is easy to get wrong by accident.
//! A short key silently absorbs a long bench name and vice versa, so renaming
//! a benchmark group can orphan a budget without touching the budget file.
//!
//! **Deliberately not asserted: the reverse direction.** A benchmark with no
//! budget is a judgement call — most benches are there to be read, not gated,
//! and demanding a budget for each would fail on every diagnostic bench in the
//! tree. Same reasoning as the metrics guard: assert what must be true, not
//! what would merely be tidy.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/common -> crates -> repo root")
        .to_path_buf()
}

/// Budget keys, by section. `max_regression_pct` is a knob, not a bench key.
fn budget_keys() -> (Vec<String>, Vec<String>) {
    let body = std::fs::read_to_string(repo_root().join("quality/benchmark-budgets.toml"))
        .expect("quality/benchmark-budgets.toml must exist");
    let (mut budgets, mut elements) = (Vec::new(), Vec::new());
    let mut section = "";
    for line in body.lines() {
        let t = line.trim();
        if t.starts_with('#') || t.is_empty() {
            continue;
        }
        if t.starts_with('[') {
            section = if t.starts_with("[budgets]") {
                "b"
            } else if t.starts_with("[elements]") {
                "e"
            } else {
                ""
            };
            continue;
        }
        let Some((lhs, _)) = t.split_once('=') else {
            continue;
        };
        let key = lhs.trim().to_string();
        match section {
            "b" => budgets.push(key),
            "e" => elements.push(key),
            _ => {}
        }
    }
    (budgets, elements)
}

/// Every Criterion id a bench can report, normalised the way `bench-gate.sh`
/// normalises them: `/` → `_`, lowercased.
fn normalised_bench_ids() -> Vec<String> {
    let mut ids = Vec::new();
    let benches_root = repo_root().join("crates");
    let mut stack = vec![benches_root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                let n = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if n != "target" {
                    stack.push(p);
                }
                continue;
            }
            let in_benches = p
                .parent()
                .and_then(|d| d.file_name())
                .and_then(|s| s.to_str())
                == Some("benches");
            if !in_benches || p.extension().and_then(|s| s.to_str()) != Some("rs") {
                continue;
            }
            let Ok(body) = std::fs::read_to_string(&p) else {
                continue;
            };
            let mut group = String::new();
            for line in body.lines() {
                for (marker, is_group) in
                    [("benchmark_group(\"", true), ("bench_function(\"", false)]
                {
                    if let Some(i) = line.find(marker) {
                        let rest = &line[i + marker.len()..];
                        if let Some(j) = rest.find('"') {
                            let name = &rest[..j];
                            if is_group {
                                group = name.to_string();
                                ids.push(name.to_string());
                            } else {
                                ids.push(name.to_string());
                                if !group.is_empty() {
                                    ids.push(format!("{group}/{name}"));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    ids.iter()
        .map(|s| s.replace('/', "_").to_lowercase())
        .collect()
}

/// `bench-gate.sh`'s match: either string containing the other.
fn matches(bench: &str, key: &str) -> bool {
    bench.contains(key) || key.contains(bench)
}

#[test]
fn every_latency_budget_reaches_at_least_one_benchmark() {
    let ids = normalised_bench_ids();
    assert!(
        ids.len() > 15,
        "only {} benchmark ids found — the scanner is broken, and a broken \
         scanner would report every budget as orphaned (or, if it returned \
         everything, none)",
        ids.len()
    );

    let (budgets, elements) = budget_keys();
    assert!(
        !budgets.is_empty(),
        "no [budgets] keys parsed — the TOML reader is broken"
    );

    let mut orphans = Vec::new();
    for (section, keys) in [("[budgets]", &budgets), ("[elements]", &elements)] {
        for key in keys {
            if !ids.iter().any(|b| matches(b, key)) {
                orphans.push(format!("{section} {key}"));
            }
        }
    }

    assert!(
        orphans.is_empty(),
        "these budget keys match NO benchmark:\n  {}\n\n\
         bench-gate.sh parses them and then never compares them against \
         anything, so each is a latency ceiling that enforces nothing while \
         reading like an enforced one. This exact thing already happened here \
         once — `should_emit_snapshot`, deleted 2026-07-17 as an orphaned \
         budget.\n\
         Matching is bidirectional substring on the normalised id, so renaming \
         a benchmark group can orphan a budget without the budget file being \
         touched.",
        orphans.join("\n  ")
    );
}

#[test]
fn claude_md_states_the_real_budget_key_count() {
    let (budgets, elements) = budget_keys();
    let total = budgets.len() + elements.len() + 1; // + max_regression_pct
    let claude = std::fs::read_to_string(repo_root().join("CLAUDE.md")).expect("CLAUDE.md");
    let line = claude
        .lines()
        .find(|l| l.contains("benchmark-budgets.toml") && l.contains("keys"))
        .expect("CLAUDE.md no longer describes benchmark-budgets.toml with a key count");
    assert!(
        line.contains(&format!("{total} keys")),
        "CLAUDE.md describes benchmark-budgets.toml as having a different \
         number of keys than it has ({total}: {} budgets + {} elements + \
         max_regression_pct).\nLine: {line}",
        budgets.len(),
        elements.len()
    );
}

#[test]
fn guard_self_test() {
    let ids = normalised_bench_ids();
    assert!(
        ids.iter().any(|b| b == "dispatch_frame_ticker"),
        "the id scanner did not build a group/function id it demonstrably \
         should have — budgets keyed on a full id would look orphaned"
    );
    assert!(
        ids.iter().all(|b| !b.contains('/')),
        "an id kept a slash — bench-gate.sh normalises those to underscores, so \
         this guard would disagree with the thing it is modelling"
    );
    // The bidirectional rule is the subtle part: a SHORT key must be able to
    // match a LONG bench id. If this ever became one-directional, most budgets
    // would silently look orphaned and someone would delete real ceilings.
    assert!(
        matches("pipeline_batch_100_mixed", "pipeline"),
        "short key no longer matches a longer bench id — the match rule has \
         drifted from bench-gate.sh"
    );
    assert!(
        !matches("registry_get_hit", "oms_state_transition"),
        "match rule accepts unrelated names, so it would never report an orphan"
    );
}

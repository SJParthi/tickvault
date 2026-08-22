//! The test-count ratchet is blind to the cheapest way to lose a test.
//!
//! `test-count-guard.sh` counts `#[test]` and `#[tokio::test]` **attributes in
//! source** and requires the total to never fall. That stops a test being
//! DELETED. It does nothing about a test being IGNORED: adding `#[ignore]`
//! above one leaves the attribute in place, so the count is unchanged, the
//! ratchet prints "Test count stable", and the test stops running.
//!
//! The house rule is *never delete, skip or disable a test to get a gate
//! green*. Deleting was enforced. Skipping was not — and skipping is the
//! easier move when a test is inconvenient at 2am.
//!
//! This is the **second** blind spot of this exact shape in that guard. Its own
//! header records the first: it counted only `#[test]`, and the string
//! `#[tokio::test]` does not contain `#[test]`, so **1,166 async tests —
//! roughly an eighth of the suite — were invisible to the ratchet** and could
//! have been deleted wholesale without it noticing. Same failure both times:
//! the count measured something *adjacent* to the thing it claimed to protect.
//!
//! Two rules here, and they do different jobs:
//!
//! 1. **Every `#[ignore]` must carry a reason.** A bare `#[ignore]` is how a
//!    test gets quietly parked — no diff comment, nothing for a reviewer to
//!    argue with. Requiring a string forces the author to say it out loud.
//! 2. **The ignored set may only shrink.** A reason string alone would let
//!    someone park a real gate behind a plausible-sounding excuse. New entries
//!    have to be added here deliberately, in the same change, where they are
//!    visible.
//!
//! The five ignored today are all genuinely not gates: two measurement
//! harnesses (wall-clock timings, which would flake on a shared CI runner and a
//! flaky gate is worse than none), a reporting aid, a re-bless helper that
//! enforces nothing, and a chaos test needing a live Docker stack.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/common -> crates -> repo root")
        .to_path_buf()
}

/// Every test allowed to be ignored, as (file, fn name). Keyed on both so a
/// reason string cannot be reused to park a different test.
///
/// This list may SHRINK freely. Growing it is a deliberate edit, in the same
/// change, with the reason visible to a reviewer.
const ALLOWED_IGNORED: &[(&str, &str)] = &[
    (
        "crates/trading/src/candles/multi_tf_aggregator.rs",
        "catch_up_seal_all_sweep_cost_at_the_authorized_ceiling",
    ),
    // Added 2026-08-22 with the fold-CPU measurement it belongs to: same
    // shape as the sweep harness above -- a wall-clock number on a shared CI
    // runner is a flake, and a flaky gate teaches people to ignore gates. It
    // is run deliberately in release, never as a merge condition.
    (
        "crates/trading/src/candles/multi_tf_aggregator.rs",
        "fold_cost_at_the_authorized_ceiling",
    ),
    (
        "crates/core/src/pipeline/tick_gap_detector.rs",
        "scan_silence_sweep_cost_at_the_authorized_ceiling",
    ),
    (
        "crates/storage/tests/critical_errcode_alarm_coverage_guard.rs",
        "report_critical_paging_coverage",
    ),
    (
        "crates/storage/tests/operator_boundary_indicator_strategy_guard.rs",
        "bless_indicator_strategy_boundary_manifest",
    ),
    (
        "crates/core/tests/chaos_cascade_triple_failure.rs",
        "cascade_01_triple_failure_live_docker_zero_loss",
    ),
];

struct Ignored {
    file: String,
    func: String,
    has_reason: bool,
}

/// Find every `#[ignore]` ATTRIBUTE and the test it sits on.
///
/// Comment lines are skipped. That is not a nicety: this workspace has nine
/// prose mentions of `#[ignore]` in doc comments explaining why things are NOT
/// ignored, and a naive scan reports them as bare ignores — which is exactly
/// what the first pass of this audit did.
fn scan_ignored() -> Vec<Ignored> {
    let root = repo_root();
    let mut out = Vec::new();
    let mut stack = vec![root.join("crates")];
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
            if p.extension().and_then(|s| s.to_str()) != Some("rs") {
                continue;
            }
            let Ok(body) = std::fs::read_to_string(&p) else {
                continue;
            };
            let rel = p
                .strip_prefix(&root)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            let lines: Vec<&str> = body.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                let t = line.trim();
                if t.starts_with("//") || !t.starts_with("#[ignore") {
                    continue;
                }
                let has_reason = t.starts_with("#[ignore =") || t.starts_with("#[ignore(");
                // The test fn is the next `fn` line, skipping further attributes.
                let func = lines[i + 1..]
                    .iter()
                    .take(6)
                    .find_map(|l| {
                        let l = l.trim();
                        let rest = l.strip_prefix("fn ").or_else(|| {
                            l.strip_prefix("async fn ")
                                .or_else(|| l.strip_prefix("pub fn "))
                        })?;
                        Some(rest.split('(').next()?.trim().to_string())
                    })
                    .unwrap_or_else(|| format!("<unparsed at {rel}:{}>", i + 1));
                out.push(Ignored {
                    file: rel.clone(),
                    func,
                    has_reason,
                });
            }
        }
    }
    out
}

#[test]
fn every_ignored_test_states_why() {
    let bare: Vec<String> = scan_ignored()
        .into_iter()
        .filter(|i| !i.has_reason)
        .map(|i| format!("{} :: {}", i.file, i.func))
        .collect();
    assert!(
        bare.is_empty(),
        "these tests are ignored with no stated reason:\n  {}\n\n\
         A bare #[ignore] parks a test with nothing for a reviewer to argue \
         with, and the test-count ratchet cannot see it — the attribute stays, \
         so the count does not move. Write #[ignore = \"why\"].",
        bare.join("\n  ")
    );
}

#[test]
fn the_ignored_set_only_shrinks() {
    let found = scan_ignored();
    assert!(
        !found.is_empty(),
        "the scanner found no #[ignore] attributes at all. There are known to \
         be several, so the scanner is broken — and a broken scanner passes \
         this test vacuously forever."
    );

    let allowed: BTreeSet<(&str, &str)> = ALLOWED_IGNORED.iter().copied().collect();
    let newly: Vec<String> = found
        .iter()
        .filter(|i| !allowed.contains(&(i.file.as_str(), i.func.as_str())))
        .map(|i| format!("{} :: {}", i.file, i.func))
        .collect();

    assert!(
        newly.is_empty(),
        "these tests are newly ignored:\n  {}\n\n\
         The house rule is never delete, skip or disable a test to get a gate \
         green. The count ratchet enforces the DELETE half only: #[ignore] \
         leaves the #[test] attribute in place, so the count never moves and \
         the guard reports stable while the test does not run.\n\
         If this test genuinely is not a gate, add it to ALLOWED_IGNORED here \
         with its reason, in the same change, where a reviewer sees it.",
        newly.join("\n  ")
    );
}

#[test]
fn the_allowlist_does_not_outlive_its_entries() {
    let found: BTreeSet<(String, String)> = scan_ignored()
        .into_iter()
        .map(|i| (i.file, i.func))
        .collect();
    let stale: Vec<String> = ALLOWED_IGNORED
        .iter()
        .filter(|(f, n)| !found.contains(&((*f).to_string(), (*n).to_string())))
        .map(|(f, n)| format!("{f} :: {n}"))
        .collect();
    assert!(
        stale.is_empty(),
        "these allowlist entries no longer match an ignored test:\n  {}\n\n\
         Good news — a test was un-ignored or removed. Drop the entry in the \
         same change, so the allowlist cannot quietly accumulate permission for \
         tests that no longer exist.",
        stale.join("\n  ")
    );
}

#[test]
fn guard_self_test() {
    let found = scan_ignored();
    assert_eq!(
        found.len(),
        ALLOWED_IGNORED.len(),
        "scanner found {} ignored tests against an allowlist of {} — the two \
         other tests here would still pass if the scanner over- or \
         under-counted in a compensating way, so this pins the total",
        found.len(),
        ALLOWED_IGNORED.len()
    );
    assert!(
        found.iter().all(|i| i.has_reason),
        "every ignored test in this workspace carries a reason today; if that \
         changed, every_ignored_test_states_why should have caught it first"
    );
    // The comment-stripping is load-bearing: this workspace has prose mentions
    // of the attribute in doc comments, and counting those would report bare
    // ignores that do not exist.
    assert!(
        found.iter().all(|i| !i.func.starts_with("<unparsed")),
        "an #[ignore] was found with no test function under it — either the \
         scanner is matching prose, or an attribute is orphaned: {:?}",
        found.iter().map(|i| &i.func).collect::<Vec<_>>()
    );
}

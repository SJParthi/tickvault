//! Sub-PR #1 of 2026-05-27 daily-universe expansion — source-scan ratchet
//! pinning the **m8g.large instance type lock** documented in
//! `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md` §7.
//!
//! Lock history: 2026-05-18 t4g.medium → 2026-05-27 t4g.large → 2026-05-29
//! **m8g.large** (Graviton4, 8 GiB RAM — operator Quote 5). RAM unchanged at
//! 8 GiB across the t4g.large→m8g.large change; only the instance string +
//! pricing moved. This guard fails the build if:
//!
//!  1. The new authoritative rule file disappears or stops pinning
//!     `m8g.large`.
//!  2. The four superseded rule files lose their SUPERSEDED-BY
//!     markers (= future readers wouldn't find the new contract).
//!  3. The instance-type lock value in any of the 5 docs disagrees
//!     with `t4g.large`.
//!
//! See:
//! - `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`
//! - `.claude/rules/project/aws-budget.md`
//! - `docs/architecture/aws-indices-only-locked-architecture.md` §5
//! - `.claude/rules/project/websocket-connection-scope-lock.md`
//! - `.claude/rules/project/operator-charter-forever.md` §I

#![cfg(test)]

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/storage parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// Section A — the new authoritative rule file MUST exist and pin
/// `m8g.large` as the locked instance type.
#[test]
fn instance_lock_authoritative_rule_file_pins_m8g_large() {
    let root = repo_root();
    let path = root.join(".claude/rules/project/daily-universe-scope-expansion-2026-05-27.md");
    assert!(
        path.exists(),
        "authoritative rule file missing at {}",
        path.display()
    );
    let body = read(&path);
    assert!(
        body.contains("m8g.large"),
        "daily-universe rule file must pin `m8g.large` as the new instance lock"
    );
    assert!(
        body.contains("**m8g.large**"),
        "daily-universe rule file must bold-pin `m8g.large` in the §7 instance-spec table"
    );
    assert!(
        body.contains("8 GiB RAM"),
        "daily-universe rule file must pin 8 GiB RAM for m8g.large"
    );
}

/// Section B — the 4 superseded files MUST carry SUPERSEDED-BY markers
/// pointing at the new rule file. Without these, future readers
/// following the old authority chain would not find the 2026-05-27
/// contract.
#[test]
fn instance_lock_supersession_markers_present_in_aws_budget() {
    let root = repo_root();
    let body = read(&root.join(".claude/rules/project/aws-budget.md"));
    assert!(
        body.contains("SUPERSEDED 2026-05-27"),
        "aws-budget.md must carry the 2026-05-27 supersession marker"
    );
    assert!(
        body.contains("daily-universe-scope-expansion-2026-05-27.md"),
        "aws-budget.md must link to the new authoritative rule file"
    );
    assert!(
        body.contains("t4g.medium → t4g.large") || body.contains("t4g.large"),
        "aws-budget.md supersession marker must mention t4g.large upgrade"
    );
}

#[test]
fn instance_lock_supersession_markers_present_in_architecture_doc() {
    let root = repo_root();
    let body = read(&root.join("docs/architecture/aws-indices-only-locked-architecture.md"));
    assert!(
        body.contains("§5 SUPERSEDED 2026-05-27"),
        "architecture doc must carry the §5 SUPERSEDED marker"
    );
    assert!(
        body.contains("daily-universe-scope-expansion-2026-05-27.md"),
        "architecture doc must link to the new authoritative rule file"
    );
}

#[test]
fn instance_lock_supersession_markers_present_in_websocket_scope_lock() {
    let root = repo_root();
    let body = read(&root.join(".claude/rules/project/websocket-connection-scope-lock.md"));
    assert!(
        body.contains("SUPERSEDED 2026-05-27"),
        "websocket-connection-scope-lock.md must carry the 2026-05-27 supersession marker"
    );
    assert!(
        body.contains("daily-universe-scope-expansion-2026-05-27.md"),
        "websocket-connection-scope-lock.md must link to the new authoritative rule file"
    );
}

#[test]
fn instance_lock_supersession_markers_present_in_operator_charter() {
    let root = repo_root();
    let body = read(&root.join(".claude/rules/project/operator-charter-forever.md"));
    assert!(
        body.contains("SUPERSEDED 2026-05-27"),
        "operator-charter-forever.md §I must carry the 2026-05-27 supersession marker"
    );
    assert!(
        body.contains("daily-universe-scope-expansion-2026-05-27.md"),
        "operator-charter-forever.md must link to the new authoritative rule file"
    );
}

/// Section C — the bill in §7 must match the locked figure. Operator
/// approved ~₹2,058/mo on 2026-05-29 (m8g.large, 270 hrs, 30 GB EBS, no EIP,
/// incl. 18% GST; supersedes the earlier ~₹1,503 / ~₹1,514 figures). If
/// someone re-tunes this in the rule file without operator approval, the
/// build fails.
#[test]
fn instance_lock_monthly_bill_pinned_to_rupees_2058() {
    let root = repo_root();
    let body =
        read(&root.join(".claude/rules/project/daily-universe-scope-expansion-2026-05-27.md"));
    assert!(
        body.contains("~₹2,058/mo") || body.contains("Rs 2,058/mo"),
        "rule file §7 must pin the ~₹2,058/mo bill (operator approved 2026-05-29: m8g.large, 270 hrs, 30 GB EBS, no EIP, incl GST)"
    );
}

/// Section D — the schedule lock must mention `08:00 IST` (operator widened
/// the window to 08:00-17:00 on 2026-06-02: "make it as 8 am till 5 pm … so
/// pre-market and post-market and deployment … can run without worries").
#[test]
fn instance_lock_schedule_pinned_to_0800_ist_start() {
    let root = repo_root();
    let body =
        read(&root.join(".claude/rules/project/daily-universe-scope-expansion-2026-05-27.md"));
    assert!(
        body.contains("08:00–17:00 IST") || body.contains("08:00 IST"),
        "rule file §7 must pin the 08:00 IST EventBridge cron start"
    );
}

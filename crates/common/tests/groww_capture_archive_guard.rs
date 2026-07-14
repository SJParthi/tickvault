//! Ratchet: the Groww sidecar capture file rotates AT OPEN (not only at an
//! in-process IST midnight the prod box never sees) and rotated archives are
//! deleted ONLY after a size-verified S3 offload.
//!
//! Disk-retention hardening (2026-07-13, prod disk-full incident): the prod
//! box runs 08:30–16:30 IST, so the original midnight-only rotation NEVER
//! fired there — `data/groww/live-ticks.ndjson` grew ~1 GB/day unbounded and
//! the 2-day archive deleter (which lived inside the rotation branch) never
//! ran either. The fix rotates a previous-IST-day live file at open and
//! replaces the blind age-delete with a verified S3 offload sweep. This guard
//! pins the contract as a build failure, not a review comment (the
//! groww_no_mint_guard.rs pattern).

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
}

const SIDECAR: &str = "scripts/groww-sidecar/groww_sidecar.py";

#[test]
fn test_rotation_at_open_is_rust_owned() {
    // Review round 1 F1+F2 redesign: rotation-at-open lives in RUST
    // (groww_bridge.rs), executed synchronously BEFORE the bridge + sidecar
    // spawns (ordering pinned by crates/app/tests/disk_retention_wiring_guard.rs)
    // and NAMED BY THE BRIDGE'S SNAPSHOT DAY — the exact archive name the
    // bridge's boot drain probes. A sidecar-side at-open rename races the
    // bridge's one-shot drain decision and can orphan the un-flushed tail,
    // so the PYTHON side must never rotate at open.
    let bridge = read("crates/app/src/groww_bridge.rs");
    assert!(
        bridge.contains("pub fn decide_capture_rotate_at_open("),
        "the at-open decision must stay a pure, unit-tested Rust gate"
    );
    assert!(
        bridge.contains("pub fn rotate_stale_groww_capture_at_open"),
        "the Rust-owned boot rotation must exist in groww_bridge.rs"
    );
    let sidecar = read(SIDECAR);
    for banned in ["_rotate_at_open_if_stale", "_should_rotate_at_open"] {
        assert!(
            !sidecar.contains(banned),
            "sidecar-side rotation-at-open ({banned}) must stay retired — \
             only Rust renames at open (F1 boot-race prevention)"
        );
    }
}

#[test]
fn test_sidecar_never_deletes_without_verified_s3() {
    let sidecar = read(SIDECAR);
    assert!(
        sidecar.contains("NEVER delete without the verified S3 copy"),
        "the no-delete-without-verified-S3 invariant marker must stay in the \
         sidecar (deletion is gated on a head_object size match)"
    );
    assert!(
        sidecar.contains("def _archive_delete_eligible("),
        "delete eligibility must stay a pure, unit-tested gate"
    );
    assert!(
        sidecar.contains("head_object"),
        "the S3 copy must be VERIFIED via head_object before any local delete"
    );
    assert!(
        sidecar.contains("ARCHIVE_DELETE_GRACE_SECS = 45 * 60"),
        "the 45-minute delete grace (protects the Rust bridge's boot-time \
         archive tail drain) must stay pinned"
    );
}

#[test]
fn test_blind_age_delete_is_retired() {
    let sidecar = read(SIDECAR);
    for banned in ["NDJSON_ARCHIVE_KEEP_DAYS", "_archives_to_delete"] {
        assert!(
            !sidecar.contains(banned),
            "the pre-2026-07-13 blind age-delete ({banned}) must stay retired — \
             the verified S3 offload sweep is the ONE archival path"
        );
    }
}

#[test]
fn test_supervisor_injects_archive_env_names() {
    let supervisor = read("crates/app/src/groww_sidecar_supervisor.rs");
    let sidecar = read(SIDECAR);
    // Split needles so rustfmt call-wrapping can't break the scan; the same
    // literal env names must appear on BOTH sides of the process boundary.
    for env_name in [
        "TICKVAULT_GROWW_ARCHIVE_S3_BUCKET",
        "TICKVAULT_GROWW_ARCHIVE_S3_PREFIX",
    ] {
        assert!(
            supervisor.contains(env_name),
            "the supervisor must inject {env_name} into the sidecar child"
        );
        assert!(
            sidecar.contains(&format!("\"{env_name}\"")),
            "the sidecar must read {env_name} for its archive sweep"
        );
    }
}

#[test]
fn test_config_carries_capture_archive_fields() {
    let config = read("crates/common/src/config.rs");
    for field in ["capture_archive_s3_bucket", "capture_archive_s3_prefix"] {
        assert!(
            config.contains(&format!("pub {field}: String")),
            "GrowwFeedTuning must carry `{field}` (serde-default empty = \
             archival off)"
        );
    }
}

#[test]
fn test_base_toml_points_at_prod_cold_bucket() {
    let toml = read("config/base.toml");
    assert!(
        toml.contains("capture_archive_s3_bucket = \"tv-prod-cold\""),
        "prod capture archives must offload to the tv-prod-cold bucket"
    );
    assert!(
        toml.contains("capture_archive_s3_prefix = \"groww-capture\""),
        "prod capture archives must live under the groww-capture prefix"
    );
}

#[test]
fn test_sidecar_sweep_runs_on_daemon_thread_and_never_logs_secrets() {
    let sidecar = read(SIDECAR);
    assert!(
        sidecar.contains("name=\"groww-archive-sweep\", daemon=True"),
        "the archive sweep must run on a DAEMON thread so it can never delay \
         feed auth/subscribe/streaming"
    );
    assert!(
        sidecar.contains("never a\n    token or credential.")
            || sidecar.contains("never a token or credential"),
        "the sweep's no-secret-logging contract marker must stay present"
    );
}

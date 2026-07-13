# Implementation Plan: Disk Retention Hardening (prod disk-full remediation)

**Status:** VERIFIED
**Date:** 2026-07-13
**Approved by:** Parthiban (operator, 2026-07-13, relayed via coordinator: "go ahead and merge everything once it is green - yes do whatever is the recommendation")

Per-item guarantee matrix: cross-reference .claude/rules/project/per-wave-guarantee-matrix.md (15-row + 7-row matrices apply; evidence in PR body).

## Context (Verified — 2026-07-13 incident)

Prod 30 GB root EBS crossed 80% used today (81.8% peak, `tv-prod-disk-used-high` ALARM since
11:44 IST), growing +2.5–3.5 GB/trading-day with ZERO reclamation (CloudWatch staircase
30% → 82% over 5 trading days). Three verified unbounded writers this plan fixes:

1. **Groww sidecar NDJSON** `data/groww/live-ticks.ndjson` — rotation is DEAD on the prod
   schedule: `_RotatingOut.__init__` (scripts/groww-sidecar/groww_sidecar.py) stamps
   `_day = today` at open; `_maybe_rotate` fires only when the IST day changes while the
   process is ALIVE. The prod box runs 08:30–16:30 IST and never crosses midnight, so the
   file has grown ~1 GB/day for weeks (est. 8–13 GB). The 2-day archive deleter lives
   INSIDE the rotation branch, so it never runs either.
2. **WAL frame-spill archive** `data/ws_wal/archive/` (crates/storage/src/ws_frame_spill.rs)
   — confirmed-replay segments are MOVED there and NEVER pruned anywhere (~0.15–0.6 GB/day).
3. **`data/logs/machine/errors.log`** — single unbounded append file explicitly skipped by
   every sweeper (crates/app/src/observability.rs `sweep_app_log_retention` skips `*.log`).

## Plan Items

- [x] Item 1 — Groww sidecar rotation-at-open + verified S3 offload (Python)
  - Files: scripts/groww-sidecar/groww_sidecar.py, scripts/groww-sidecar/test_capture_archive.py
  - Tests: test_should_rotate_at_open_previous_day, test_should_rotate_at_open_same_day,
    test_collision_free_archive_path_suffixes, test_archive_delete_eligible_requires_s3_verified,
    test_archive_delete_eligible_requires_both_graces, test_rotate_at_open_moves_stale_file,
    test_rotate_at_open_leaves_today_file (python3 -m unittest, scripts/groww-sidecar/)
- [x] Item 2 — Supervisor env plumbing + config fields (Rust, crates/app + crates/common)
  - Files: crates/app/src/groww_sidecar_supervisor.rs, crates/common/src/config.rs,
    crates/app/src/main.rs, config/base.toml
  - Tests: test_supervisor_injects_archive_s3_env (crates/app),
    test_feeds_groww_capture_archive_defaults_empty + test_feeds_groww_capture_archive_round_trip
    (crates/common config tests)
- [x] Item 3 — Sidecar behavior ratchet guard (Rust source-scan, crates/common)
  - Files: crates/common/tests/groww_capture_archive_guard.rs
  - Tests: test_sidecar_has_rotation_at_open, test_sidecar_never_deletes_without_verified_s3,
    test_supervisor_injects_archive_env_names, test_config_carries_capture_archive_fields,
    test_base_toml_points_at_prod_cold_bucket, test_blind_age_delete_is_retired
- [x] Item 4 — WAL frame-spill archive pruning (Rust, crates/storage + crates/app)
  - Files: crates/storage/src/ws_frame_spill.rs, crates/common/src/constants.rs,
    crates/app/src/main.rs
  - Tests: test_prune_preserves_fresh_archive_segments, test_prune_deletes_old_archive_segments,
    test_prune_ignores_non_segment_names, test_prune_missing_dir_is_noop (crates/storage);
    wiring ratchet in crates/app/tests/disk_retention_wiring_guard.rs
- [x] Item 5 — errors.log size cap (Rust, crates/app)
  - Files: crates/app/src/observability.rs, crates/app/src/main.rs
  - Tests: test_cap_errors_log_under_limit_untouched, test_cap_errors_log_over_limit_truncated,
    test_cap_errors_log_missing_file_ok (crates/app observability tests);
    wiring ratchet in crates/app/tests/disk_retention_wiring_guard.rs
- [x] Item 6 — Wiring ratchet (Rust source-scan, crates/app)
  - Files: crates/app/tests/disk_retention_wiring_guard.rs
  - Tests: test_main_spawns_ws_wal_archive_prune, test_main_wires_errors_log_cap

## Design

**Item 1 (sidecar).** Two additions to the existing `_RotatingOut` machinery, keeping the
writer-owns-rotation model:

- *Rotation-at-open:* before opening the live file, `_RotatingOut.__init__` calls
  `_rotate_at_open_if_stale(path)` — one `os.stat`; if the file exists, is non-empty, and its
  mtime's IST day (`_ist_day(mtime)`) is BEFORE today's IST day (pure gate
  `_should_rotate_at_open(mtime_day, today_day)`), it is `os.replace`d to the SAME dated
  archive name the midnight rotation uses (`live-ticks-YYYYMMDD.ndjson`, stamped with the
  mtime's day = the last-write day, honest). On a name collision a numeric suffix is appended
  (`-1`, `-2`, …; pure `_collision_free_archive_path`) — never overwrite. Cheap: a stat, no
  file scan. The fresh live file then opens exactly as today.
- *Verified S3 offload:* after any rotation (at-open or midnight) and once at startup, a
  single-flight DAEMON thread (`_spawn_archive_sweep`) sweeps `live-ticks-*.ndjson` archives:
  upload each to `s3://$TICKVAULT_GROWW_ARCHIVE_S3_BUCKET/$TICKVAULT_GROWW_ARCHIVE_S3_PREFIX/<filename>`
  via boto3 (already pinned for the SSM token read), then VERIFY via `head_object`
  ContentLength == local size. After a 45-minute grace sleep the delete pass removes each
  archive ONLY when the pure `_archive_delete_eligible(s3_verified, age_secs, uptime_secs,
  grace_secs)` gate passes: (1) S3-verified at delete time, (2) file mtime age ≥
  `ARCHIVE_DELETE_GRACE_SECS` (45 min), (3) process uptime ≥ the same grace. NEVER delete
  without the verified S3 copy — no bucket configured / creds missing / upload or verify
  failure ⇒ the file is KEPT and one bounded warning per file per session is printed. Failed
  uploads retry at the next trigger (next startup / next midnight rotation), never in a loop.
  Re-runs are idempotent (same key, overwrite fine; already-verified objects skip re-upload).
  The old blind 2-day age-delete (`_archives_to_delete` + `NDJSON_ARCHIVE_KEEP_DAYS`) is
  REMOVED — verified-offload-then-delete is the ONE archival path.
- *Bridge safety (the grace rationale, Verified from crates/app/src/groww_bridge.rs):* the
  bridge persists a flushed-offset snapshot carrying `ist_day`; at BOOT, when the snapshot
  belongs to a previous IST day, `drain_archive_tail_if_needed` re-reads the rotated archive
  `live-ticks-<snapshot-day>.ndjson` tail (bytes past the flushed offset) through the persist
  path. The archive's mtime is stale (yesterday ~16:30), so an mtime-only grace passes
  instantly at the 08:31 at-open rotation — hence the ADDITIONAL process-uptime ≥ 45 min
  condition: the sidecar is spawned by the supervisor inside the same app process as the
  bridge, so 45 min of sidecar uptime guarantees the co-booted bridge finished its one-shot
  boot archive drain long before any delete fires. After that drain the bridge's next flushed
  snapshot carries TODAY's day and the archive is never consulted again.
- The 15:40 conservation audit (`count_groww_ndjson_lines_for_ist_day`) reads ONLY the live
  path filtered to today's timestamps — rotation/offload of previous-day content cannot
  change today's count (and shrinks the daily scan from ~10 GB to one day).

**Item 2 (plumbing).** `crates/common/src/config.rs::GrowwFeedTuning` gains
`capture_archive_s3_bucket: String` + `capture_archive_s3_prefix: String`, both
`#[serde(default)]` (empty = archival off, dev-Mac behavior unchanged).
`config/base.toml` `[feeds.groww]` sets bucket `tv-prod-cold`, prefix `groww-capture`.
`GrowwSidecarOptions` carries the two values (copied by `shard_sidecar_options` for §34 fleet
children) and the spawn injects `TICKVAULT_GROWW_ARCHIVE_S3_BUCKET` /
`TICKVAULT_GROWW_ARCHIVE_S3_PREFIX` into the child env (path/name strings only — never a
credential). main.rs builds the options from `config.feeds.groww` at both spawn sites.

**Item 4 (WAL prune).** `crates/storage/src/ws_frame_spill.rs` gains
`prune_archived_segments_at(wal_dir, retention_secs, now)` (pure-testable core over an
injected `now`) + `prune_archived_segments` wrapper: deletes `<wal_dir>/archive/*.wal` files
with mtime older than `WS_WAL_ARCHIVE_RETENTION_SECS` (172_800 s = 2 days, new constant in
crates/common/src/constants.rs). Rationale: archived segments are post-confirmed-replay
copies — their frames are already durably persisted + replay-confirmed; the same-day
tick-conservation audit reads only the CURRENT day's frames (3-day segment-creation lookback
pre-filter, day-attributed counting), and 2 days comfortably exceeds that window. Spawned as
a new periodic tokio task in the process-global boot prefix of main.rs next to the existing
log-retention sweeper family (runs on BOTH boot arms — deliberately NOT inside the Dhan-lane
periodic health loop, which never runs on a Groww-only boot): prune once at task start, then
every 6 h. Observability: one `info!` summary line per pass that deleted anything + the
`tv_ws_wal_archive_pruned_total` counter (checked: the Prometheus-side
resilience_sla_alert_guard.rs / operator_health_dashboard_guard.rs meta-guards were RETIRED
with the CloudWatch-only migration and no longer exist in the tree, so a counter forces no
alert/panel; it stays /metrics-local, not CloudWatch-shipped — ₹0). Deletion failures are
NOT persist/flush failures: `warn!` per failed file within the 6-hourly pass (matching the
existing sweeper style in observability.rs).

**Item 5 (errors.log cap).** `crates/app/src/observability.rs` gains
`ERRORS_LOG_MAX_BYTES` (512 MiB) + `cap_errors_log_size(path, max_bytes)`: if the file
exceeds the cap, truncate to 0 via `set_len(0)` (safe against the live O_APPEND writer —
every append repositions at EOF). Nothing is uniquely lost: the same WARN+ lines also exist
in the hourly machine app logs and errors.jsonl. Hosted inside the EXISTING hourly
errors.jsonl retention sweeper task in main.rs (no new task). Canonical path names untouched
(`boot_helpers::ERROR_LOG_FILE_PATH`).

## Edge Cases

- Sidecar restart mid-day (stall watchdog kill+relaunch): the live file's mtime is TODAY →
  `_should_rotate_at_open` false → no rotation, append continues (capture_seq/bridge tail
  unaffected).
- Live file exists but empty at open → no rotation (size > 0 required); nothing to archive.
- Archive name collision (crash between rotate and open, or two rotations attributing the
  same day) → numeric suffix, never overwrite; both files sweep to S3 under distinct keys.
- Dev Mac without AWS creds / empty bucket config → upload never verifies → NO deletion ever
  (archives accumulate on dev; accepted per the no-delete-without-verified-S3 rule; one
  bounded warning per file per session).
- Midnight rotation while process alive (dev): archive mtime ≈ midnight → mtime grace holds
  deletion for 45 min; the in-process bridge needs no archive (market closed at midnight —
  documented in the existing rotation comment).
- Prod 16:30 box stop before the 45-min grace elapses after a late trigger → delete simply
  happens on the NEXT morning's startup sweep (upload already verified; idempotent).
- S3 object exists from a previous session (uploaded but not yet deleted) → head_object
  matches → upload skipped, delete proceeds after grace.
- WAL prune: `archive/` missing → no-op Ok; non-`.wal` names ignored; segments moved into
  archive/ today by `confirm_replayed` keep their append-time mtime (rename preserves mtime)
  — they were created ≤ 1 day ago on the daily-restart schedule, so well inside retention.
- errors.log absent (fresh boot) → cap returns Ok(false), no create.
- errors.log exactly at the cap → untouched (strictly-greater comparison).

## Failure Modes

- boto3 import/credential/endpoint failure → per-file bounded warning, file KEPT, retry at
  next trigger. Capture (auth/subscribe/streaming) is never delayed: the sweep runs on a
  daemon thread.
- `head_object` size mismatch (truncated upload) → NOT verified → file KEPT, re-uploaded next
  trigger.
- `os.replace` failure at-open (bad disk) → warning, capture continues appending to the
  existing file (exactly the midnight-rotation failure semantics).
- Sweep thread crash → daemon thread dies silently for THIS session; next startup re-triggers
  (bounded blast radius: archives are kept, never lost).
- WAL prune `remove_file` failure → `warn!` + counted in the outcome; retried next 6 h pass.
- errors.log truncate failure (permissions) → `warn!` once per hourly pass; file keeps
  growing until fixed — visible, never silent.
- Race: prune scanning `archive/` while `confirm_replayed` renames into it → both are
  metadata ops on distinct files; a just-moved segment's mtime is fresh relative to the 2-day
  cutoff, so it is never deleted mid-move-window.

## Test Plan

- Python: `python3 -m unittest discover scripts/groww-sidecar -p 'test_*.py'` — new
  test_capture_archive.py covers the pure gates (`_should_rotate_at_open`,
  `_archive_delete_eligible`, `_collision_free_archive_path`) + a tmpdir end-to-end
  rotate-at-open test using `os.utime` to backdate mtime; existing test_dedup.py must stay
  green. `python3 -m py_compile scripts/groww-sidecar/groww_sidecar.py`.
- Rust: `cargo test -p tickvault-common -p tickvault-storage -p tickvault-app -p
  tickvault-core` (crates/common changed → escalation; CI runs the full battery).
  New tests: config round-trip (common), groww_capture_archive_guard (common, source-scan),
  prune unit tests with tempdir (storage), cap_errors_log tests (app), supervisor env
  injection ratchet (app), disk_retention_wiring_guard (app, source-scan on main.rs).
- Gates: `cargo fmt --all -- --check`, `cargo clippy --workspace --no-deps -- -D warnings
  -W clippy::perf`, `bash .claude/hooks/banned-pattern-scanner.sh`,
  `bash .claude/hooks/plan-verify.sh`.

## Rollback

- Sidecar: revert scripts/groww-sidecar/groww_sidecar.py to the prior midnight-only rotation
  (single-file change, no schema/state on disk beyond dated archives which the bridge already
  understands). Setting `capture_archive_s3_bucket = ""` in config disables S3
  offload+deletion at runtime without a code revert (archives then accumulate = pre-PR
  behavior).
- WAL prune: the task is additive; revert the main.rs spawn (or the whole commit) — no state
  migration; already-deleted archives were confirmed-replayed copies whose frames are in
  QuestDB.
- errors.log cap: revert the sweeper-task call; the constant/function are additive.
- Whole-PR rollback is a clean `git revert` — no DB schema changes, no DEDUP-key changes,
  no config keys required by other code (all `#[serde(default)]`).

## Observability

- Sidecar prints (stdout → supervisor → tracing → CloudWatch): one line per at-open rotation,
  per archive upload (filename + size + key ONLY — never a token/credential), per verified
  delete, and one bounded warning per kept file per session.
- Rust: `tv_ws_wal_archive_pruned_total` counter + one `info!` summary per pruning pass that
  deleted files; `warn!` per deletion failure. errors.log cap: `info!` on truncation (with
  the pre-truncation size), `warn!` on failure. All through the existing 5-sink tracing chain.
- Ratchets (build-failing): groww_capture_archive_guard.rs pins rotation-at-open, the
  no-delete-without-verified-S3 literal, the env-var names, the config fields, and the
  retired blind age-delete; disk_retention_wiring_guard.rs pins the main.rs prune spawn +
  errors.log cap wiring.
- Honest envelope: this PR stops the three verified unbounded writers; it does NOT change
  QuestDB partition retention (detach ≠ free; separate operator decision per the audit) and
  does NOT claim S3 offload works without AWS creds (dev accumulates, stated above).

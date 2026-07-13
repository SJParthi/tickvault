# Implementation Plan: QuestDB partition archive→verify→drop — retention that actually frees disk

**Status:** APPROVED
**Date:** 2026-07-13
**Approved by:** Parthiban (operator, 2026-07-13, relayed via coordinator: "go ahead and merge everything once it is green - yes do whatever is the recommendation")

Per-item guarantee matrix: cross-reference .claude/rules/project/per-wave-guarantee-matrix.md.

> **Incident context (Verified, 2026-07-13):** prod 30 GB root EBS at 82% and
> climbing ~1.9–3.6 GB per trading day; the daily partition manager logs
> `total_detached=0` on every run (retention_days=90 on a ~7-week-old dataset —
> nothing qualifies), and DETACH only renames partition dirs INSIDE the same
> volume anyway. `s3://tv-prod-cold` has never received one partition object.
> The instance role ALREADY has s3:GetObject/PutObject/ListBucket on the whole
> `tv-<env>-cold` bucket (deploy/aws/terraform/main.tf — no IAM change). 90
> days of ticks (~135+ GB) can never fit the volume: the hot window itself must
> shrink for high-volume tables, with S3 as the durable long-term store (house
> doctrine: hot window on EBS, >window → S3; SEBI retention satisfied by the
> S3 copy — aws-budget.md §5 / daily-universe lock §7 Rule 3).

## Plan Items

- [x] Item 1 — Config: extend `[partition_retention]` with `archive_enabled`
  (serde default **false** — the destructive leg is explicitly configured on),
  `market_data_hot_days` (serde default 14 — safe because the flow is
  fail-closed: nothing is dropped unless verified in S3, and `archive_enabled`
  default-off means the value is inert unless the operator opts in),
  `archive_bucket` (serde default "" = derive `tv-<env>-cold` from
  TV_ENVIRONMENT), `max_partitions_per_run` (serde default 200 — bounds each
  daily run so the first catch-up sweep converges over a few evenings instead
  of overrunning the 16:30 IST box stop). Set `archive_enabled = true` +
  `market_data_hot_days = 14` in config/base.toml.
  - Files: crates/common/src/config.rs, config/base.toml
  - Tests: test_partition_retention_serde_defaults_backward_compatible,
    test_partition_retention_archive_enabled_default_false,
    test_partition_retention_full_section_parses

- [x] Item 2 — Archive engine: new `crates/storage/src/partition_archive.rs` —
  per eligible (table, partition): export full rows via QuestDB HTTP `/exp`
  (CSV, explicit designated-ts range predicates), stream + gzip to a temp file
  under `data/tmp/partition-archive/` (cleaned at run start), upload to
  `s3://<bucket>/questdb-partitions/<table>/<partition>.csv.gz`, VERIFY
  (recount == exported rows AND S3 head ContentLength == local gzip size),
  and only then `ALTER TABLE … DROP PARTITION LIST`. Drop takes a
  `VerifiedArchive` type-state proof struct constructible ONLY by the verify
  path (compile-time guarantee). Any failure = keep partition + `error!` with
  `code = ErrorCode::StorageGap04S3ArchiveFailed` + skip (bounded — next
  daily run retries; no retry storm). MIN_HOT_DAYS=2 const floor: today's and
  yesterday's partitions are untouchable regardless of config.
  - Files: crates/storage/src/partition_archive.rs, crates/storage/src/lib.rs,
    crates/storage/src/partition_manager.rs (retention-class mapping + shared
    partition selection), Cargo.toml (aws-sdk-s3 + flate2 exact workspace
    pins), crates/storage/Cargo.toml
  - Tests: test_effective_hot_days_* (boundary: exactly hot_days, hot_days-1,
    MIN_HOT_DAYS floor), test_partition_range_bounds_* (DAY/HOUR, month/year
    rollover, invalid), test_build_export_sql_uses_explicit_range,
    test_build_count_sql_uses_explicit_range,
    test_verify_decision_* (every failure combination → keep partition),
    test_retention_class_* (ticks/candles→market class, audits→standard),
    test_min_hot_days_floor_const

- [x] Item 3 — Audit trail: new `partition_archive_audit` QuestDB table
  (audit-table template: `ts` designated + in DEDUP key, `feed` in DEDUP key
  per the 2026-06-28 operator override, DEDUP
  `(ts, table_name, partition_name, feed)`), one row per attempt outcome
  (verified / dropped / export_failed / count_mismatch / size_mismatch /
  upload_failed / drop_failed). Audit-first: the `verified` row is written
  BEFORE the destructive DROP. Table added to DAY_PARTITIONED_TABLES (its own
  retention decision — the coverage guard requires it).
  - Files: crates/storage/src/partition_archive.rs,
    crates/storage/src/partition_manager.rs
  - Tests: test_archive_audit_ddl_contains_expected_columns,
    test_archive_audit_dedup_key_has_ts_and_feed,
    test_archive_outcome_labels_stable

- [x] Item 4 — Wiring: main.rs post-market block runs the archive cycle
  (when `archive_enabled`) BEFORE the existing detach cycle so a >90d
  partition is archived+dropped rather than detached-unarchived; the existing
  detach cycle (and its `total_detached=0` log lines) keeps firing unchanged
  so the CW evidence trail continues. `archive_enabled=false` (config
  rollback) restores today's detach-only behaviour byte-identically.
  - Files: crates/app/src/main.rs
  - Tests: guard test (source-scan) that main.rs still calls
    `detach_old_partitions` AND gates the archive call on `archive_enabled`

- [x] Item 5 — Guards/ratchets: archive_enabled default false in code + true
  in config/base.toml; DROP unreachable without the VerifiedArchive proof
  (type-state, plus a source-scan that the only `DROP PARTITION` emit site
  takes the proof struct); observability counters
  (tv_partition_archived_total / tv_partition_archive_verified_total /
  tv_partition_dropped_total / tv_partition_archive_failed_total) + one
  coalesced per-run info! summary.
  - Files: crates/storage/tests/partition_archive_guard.rs
  - Tests: archive_enabled_default_is_false_in_code,
    base_toml_enables_archive, main_rs_wires_archive_before_detach,
    drop_partition_requires_verified_archive_proof

## Design

Flow per eligible (table, partition), strictly ordered **archive → verify →
drop**:

1. **Eligibility** — partitions listed via QuestDB `table_partitions('<t>')`
   (the mechanism `partition_manager.rs` already uses), server-side cutoff
   `minTimestamp < dateadd('d', -N, now())` where N = the table's
   retention-class hot window clamped to `max(N, MIN_HOT_DAYS=2)`; the active
   partition is always excluded (reuse of the pure
   `select_partitions_to_detach` predicate + `is_valid_partition_name`
   fail-closed allowlist). Retention classes: **market-data**
   (`ticks` + the 21 `candles_*` tables from `candle_table_names()`) →
   `market_data_hot_days` (default 14); **everything else** (the DAY audit /
   daily-data list) → `retention_days` (existing 90). RETENTION_EXEMPT_TABLES
   are never touched.
2. **Export** — `GET /exp?query=SELECT * FROM <t> WHERE ts >= '<start>' AND
   ts < '<end>'` (explicit ISO range predicates derived from the partition
   name — DAY `YYYY-MM-DD` → [midnight, next midnight), HOUR `YYYY-MM-DDTHH`
   → [hour, next hour); never integer literals, never `IN (SELECT…)`),
   streamed chunk-by-chunk through a gzip encoder into
   `data/tmp/partition-archive/<t>-<p>.csv.gz`, counting `\n` in the raw
   (pre-compression) stream — rows = newlines − 1 header.
3. **Upload** — `PutObject` to `s3://<bucket>/questdb-partitions/<t>/<p>.csv.gz`
   (bucket = config override or derived `tv-<TV_ENVIRONMENT|prod>-cold`; the
   instance role already carries Put/Get/List on it). Single PutObject —
   partitions gzip well under the 5 GB single-put bound (an hour of ticks ≈
   tens of MB compressed).
4. **Verify (fail-closed, ALL of)** — (a) `SELECT count() FROM <t> WHERE
   <range>` taken AFTER the export equals the exported row count (partitions
   are ≥2 days old so the count is stable; any late row → mismatch → keep);
   (b) `HeadObject` exists and ContentLength == the local gzip file length;
   (c) the raw-stream newline count is the cheaper byte-equivalent of
   decompressing the file and counting lines — the counted bytes are exactly
   the bytes fed to the encoder, so no second pass is needed. Only a passing
   verify constructs the `VerifiedArchive` proof (private-field type-state —
   the drop fn cannot be called without it, checked by the compiler).
5. **Audit-first + drop** — append the `verified` audit row (best-effort ILP,
   AUDIT-WS-01-class: a write failure logs STORAGE-GAP-04-adjacent error but
   never gates), then `ALTER TABLE <t> DROP PARTITION LIST '<p>'` via /exec —
   this FREES disk. A `dropped` row records completion.
6. **Per-run bound** — at most `max_partitions_per_run` (default 200)
   partitions per daily run, oldest first, so the first catch-up run is
   bounded and converges over a few evenings.

When `archive_enabled=false` (the serde default): NONE of the above runs —
the post-market path is byte-identical to today (detach-only, 90d).

## Edge Cases

- Partition exactly `hot_days` old vs `hot_days − 1`: server-side
  `minTimestamp < dateadd('d', -N, now())` boundary; unit tests pin the pure
  clamp + class mapping; the QuestDB comparison itself is the same one the
  live detach path has used since #1451.
- `market_data_hot_days = 0` or `1`: clamped up to MIN_HOT_DAYS=2 — today's
  and yesterday's partitions can never be selected regardless of config.
- Active partition: never selected (existing predicate reused).
- 0-row partition (`/exp` returns only the header): rows=0, recount=0 →
  verify passes → dropped (an empty partition still occupies dir inodes).
- Rows containing embedded newlines inside quoted CSV fields would inflate the
  stream line count vs `count()` → verify FAILS → partition retained forever
  (safe direction; market-data tables have no free-text columns; audit-table
  reason strings are control-char-sanitized upstream).
- Hostile/corrupt partition name from a MITM'd response: rejected by the
  existing `is_valid_partition_name` allowlist before any SQL interpolation.
- Re-run after a drop failure: the S3 object is overwritten idempotently
  (same key) and the flow re-verifies before retrying the drop.
- Run overruns the 16:30 IST box stop: partitions already dropped are freed;
  the remainder is retried next run (oldest-first = monotonic progress).
- QuestDB `now()` is UTC while `ts` values are IST-labeled: the cutoff is
  ~5.5h conservative (a partition qualifies slightly later) — same semantics
  the live detach path already has; safe direction.

## Failure Modes

Every failure keeps the partition and moves on (bounded — the next daily run
retries; never a retry loop inside a run):

- `/exp` export non-2xx / stream error → `export_failed` audit row +
  `error!(code = STORAGE-GAP-04)`; temp file removed.
- Recount SQL failure or count mismatch → `count_mismatch`; keep.
- S3 PutObject failure (creds, network, bucket) → `upload_failed`; keep.
- HeadObject missing / ContentLength mismatch → `size_mismatch`; keep.
- DROP DDL failure after verify → `drop_failed`; the S3 copy already exists;
  next run re-archives (idempotent overwrite) and retries.
- Audit-row ILP write failure → best-effort `error!`; the flow itself is not
  gated (AUDIT-WS-01 class), and the S3 object + partition absence remain the
  ground-truth evidence.
- aws-config credential resolution failure at construction → archive cycle
  skipped for the run with one `error!(code = STORAGE-GAP-04)`; detach cycle
  still runs (no behaviour regression).

## Test Plan

- Pure-function unit tests (no QuestDB/S3): eligibility windows (exact
  boundary, −1, MIN_HOT_DAYS floor override), partition-range builders
  (DAY/HOUR, month + year rollover, malformed names → None), export/count/
  drop SQL builders (explicit range predicates, no `IN (SELECT`), verify
  decision matrix (each of the 3 checks failing alone and in combination →
  Err; all passing → VerifiedArchive), retention-class mapping (ticks +
  every `candle_table_names()` entry → MarketData; each DAY-list audit table
  → Standard; exempt tables → never eligible), config serde defaults
  (missing keys, archive_enabled default false), audit DDL/DEDUP-key/outcome
  labels, gzip-count equivalence (round-trip a synthetic CSV through the
  encoder and re-read).
- Guard tests: main.rs still spawns the detach cycle + gates the archive
  cycle on `archive_enabled`; base.toml sets `archive_enabled = true`; the
  sole DROP-PARTITION emit site takes `VerifiedArchive`.
- TEST-EXEMPT (house convention): the live `/exp` + S3 legs need creds + a
  live QuestDB — first prod run is the probe; the fail-closed design makes a
  failed probe a loud no-op, never a drop.
- `cargo test -p tickvault-storage -p tickvault-common -p tickvault-app`,
  fmt, clippy -D warnings, banned-pattern scanner, plan-verify.

## Rollback

Config-only, instant: set `archive_enabled = false` in
`config/base.toml` (or delete the key — serde default is false) and restart.
That restores today's exact behaviour: detach-only at `retention_days=90`,
`market_data_hot_days` inert, no S3 calls, no drops. Already-archived
partitions remain durably in S3 (`questdb-partitions/` prefix) and can be
re-imported from the CSV export if ever needed. Code rollback = revert the
single squash commit; no schema migration to unwind (the audit table is
additive, `CREATE TABLE IF NOT EXISTS`).

## Observability

- Counters (post-market cold path, no labels or static labels only):
  `tv_partition_archived_total`, `tv_partition_archive_verified_total`,
  `tv_partition_dropped_total`, `tv_partition_archive_failed_total{stage}`.
  Note: the Prometheus-side alert/panel meta-guards
  (resilience_sla_alert_guard / operator_health_dashboard_guard) were retired
  with the CloudWatch-only migration; these counters are /metrics-local +
  structured logs (the same class as the existing
  `tv_partition_detach_total`) — a CloudWatch filter/alarm is a flagged
  follow-up, consistent with the cw-findings observability-gap note.
- One coalesced `info!` per run: tables scanned, partitions archived /
  dropped / failed, bytes uploaded, disk-bytes-freed estimate.
- Every failure arm: `error!` with
  `code = ErrorCode::StorageGap04S3ArchiveFailed.code_str()` (the existing
  STORAGE-GAP-04 runbook in wave-2-error-codes.md covers triage).
- The existing detach-cycle log lines (`starting partition detach cycle` /
  `partition detach cycle complete total_detached=…` / `post-market partition
  detach complete`) keep firing unchanged — the CloudWatch evidence trail
  from cw-findings.md §4 continues.
- Forensic system-of-record: the `partition_archive_audit` table (one row per
  attempt outcome, 5y-class like its audit siblings).

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | archive_enabled=false (default) | byte-identical to today: detach-only, no S3 calls |
| 2 | 20-day-old ticks HOUR partition, archive on | export → upload → verify → audit `verified` → DROP → disk freed |
| 3 | 20-day-old `ws_event_audit` DAY partition | NOT eligible (standard class, 90d window) |
| 4 | 100-day-old audit partition | archive→verify→drop through the same flow |
| 5 | recount ≠ exported rows | keep partition, `count_mismatch` audit row, error! STORAGE-GAP-04 |
| 6 | S3 unreachable | keep partition, `upload_failed`, error!; retried next run |
| 7 | DROP DDL rejected | S3 copy retained, `drop_failed`, retried next run |
| 8 | yesterday's partition + market_data_hot_days=0 | never selected (MIN_HOT_DAYS=2 floor) |
| 9 | first catch-up run with ~1000 eligible partitions | bounded at max_partitions_per_run=200 oldest-first; converges over runs |
| 10 | config rollback mid-incident | archive_enabled=false restores detach-only instantly |

## Review Round 1 Fixes (2026-07-13, adversarial review on PR #1504)

| # | Sev | Finding | Resolution |
|---|---|---|---|
| F1a | HIGH | Unconditional S3 PutObject could OVERWRITE the only copy of already-dropped data (interrupted-run re-archive with drifted content) | `process_one` now HeadObjects BEFORE any upload: same length → REUSE (no re-upload); different length → loud `S3Conflict` (`error!` STORAGE-GAP-04, partition kept, object untouched); absent → conditional `PutObject` with `If-None-Match: "*"` (a lost race is a loud 412, never a silent overwrite) |
| F1b | HIGH | Empty `archive_bucket` + unset env vars silently defaulted to the PROD cold bucket (`tv-prod-cold`) — a dev box could write/overwrite prod objects | `resolve_environment_from`/`resolve_archive_bucket` are now `Option`-based with NO `"prod"` default; `PartitionArchiver::new` returns `Ok(None)` (archival SKIPPED, actionable warn) when no explicit bucket or env var exists; main.rs handles the skip arm |
| F2 | MEDIUM | "Durable verified audit row before drop" was FALSE: ILP TCP is fire-and-forget (2026-07-05 lesson) and the drop proceeded even when the audit write failed | Audit writer moved to ILP-over-HTTP (`partition_archive_audit_ilp_http_conf`, per-flush server ACK, `retry_timeout=0`); `append_verified_audit_gated` GATES the drop on a flushed ACK — flush failure = `AuditFailed`, partition KEPT; `discard_pending` poisoned-buffer defense on every failed flush; failure rows also flush per-row |
| F3 | MEDIUM | A WAL-SUSPENDED table's export/recount see only APPLIED rows — dropping its partitions destroys ACKed-but-unapplied rows on RESUME WAL | `fetch_wal_suspended_tables` probes `wal_tables()` FIRST (reusing `wal_suspension_watcher::parse_wal_tables_response`); suspended table → skipped table-wide with warn; probe failure → the ENTIRE run is skipped fail-closed |
| F4 | MEDIUM | The verify decision's INPUTS were not ratcheted — a refactor stubbing `recount`/`s3_len` would make the verify vacuous | New guard `verify_inputs_come_from_real_recount_and_head_calls` (brace-matched `process_one` body scan: real `recount_rows(`/`head_object_len(` calls feed `verify_archive`, self-satisfied inputs banned) + `extract_fn_body_self_test_is_not_vacuous` |
| F5 | MEDIUM | Raw `\n` counting over-counts CSV records on STRING fields with embedded newlines (this module's own audit `detail`) → permanent recount mismatch livelock | `CsvRecordCounter` quote-aware record counting (RFC-4180; `""` escapes safe; trailing-newline-optional; chunk-split-safe — proven by an exhaustive split-point test) |
| SEC-LOW1 | LOW | Audit `detail` + logged HTTP bodies carried raw server text | All details/bodies routed through `tickvault_common::sanitize::capture_rest_error_body` (control-char strip, credential/JWT redaction, 300-char cap) |
| SEC-LOW2 | LOW | Unbounded `.text()` reads of `/exec` responses | `read_body_capped` (4 MiB `ARCHIVE_MAX_EXEC_BODY_BYTES`) on every `/exec` body |
| — | — | Coverage: storage 89.67% < 91.2% floor on the old head | Added the `stub_integration_tests` module: 9 end-to-end tests driving the FULL archive→verify→drop orchestration against local QuestDB(/exec+/exp)/ILP-HTTP/S3 stubs (full flow, recount mismatch, F1 conflict + reuse, F2 drop-withheld, F3 suspended + probe-fail, empty export, per-run cap) via a test-injection `from_parts` constructor. Floor NOT lowered. |

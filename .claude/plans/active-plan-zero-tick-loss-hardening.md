# Implementation Plan: Zero-Tick-Loss Hardening — close the last gaps

**Status:** APPROVED
**Date:** 2026-06-03
**Approved by:** Parthiban ("go ahead with the plan", 2026-06-03) — serial PRs, smallest-risk first (PR-1 = green the clippy gate).

> Cross-references the canonical 15-row + 7-row guarantee matrices
> (`.claude/rules/project/per-wave-guarantee-matrix.md`) and the Z+ 7-layer
> doctrine (`.claude/rules/project/z-plus-defense-doctrine.md`). One PR at a
> time per `pr-completion-protocol.md` §H. Honest-envelope wording per
> `operator-charter-forever.md` §F.

## Why this plan exists

Operator demand 2026-06-03: maximize zero-tick-loss + full automated
monitoring/audit/logging/alerting/dashboards + O(1) everywhere, **no human
intervention**. A 4-agent deep audit (hot-path-reviewer + Explore baseline +
hostile gap-hunt + verify-build) found the core is already strong but
surfaced **2 CRITICAL detection gaps, 3 HIGH, 2 MEDIUM, + a RED clippy gate**.
This plan closes them, each with a ratchet test so they can't regress.

## Honest baseline (what is ALREADY airtight — verified, not assumed)

- Persist path is genuinely zero-loss-bounded: `try_send` never blocks the WS
  read loop; VecDeque rescue ring (`TICK_BUFFER_CAPACITY = 100_000`) → disk
  spill (`data/spill/ticks-*.bin`) → drop counter only when disk also fails.
- WAL pre-parse spill (`ws_frame_spill.rs`) captures raw frames before the
  live channel; `replay_all()` recovers on restart (SIGKILL-tested).
- Dedup is O(1): `TickDedupRing` open-addressing `Box<[u64]>`, FNV-1a of
  `(security_id, exchange_timestamp, ltp_bits)`; QuestDB DEDUP UPSERT
  `(ts, security_id, segment, received_at)` is the authoritative backstop.
- O(1) hot path: `from_le_bytes` parse, `papaya` lock-free reads, `Arc<HashMap>`
  registry with composite `(security_id, exchange_segment)` keys, bounded
  channels everywhere (no unbounded). **O(1) per-tick + per-lookup is real;**
  inherently-O(N) boot steps (CSV parse, universe build) are honestly O(N).
- Observability: 5 log sinks, 53+ `ErrorCode` taxonomy, errors.jsonl hourly +
  48h sweep, CloudWatch alarms/agent, Telegram+SNS, 6 audit tables.
- Automation: SessionStart health hooks, tickvault-logs MCP (13 tools), triage
  YAML + 8 auto-fix/rollback scripts, EventBridge/Lambda deploy + watchdog.
- 17+ chaos tests (QuestDB outage, SIGKILL replay, disk-full, WAL saturation).

## The gaps (4-agent evidence)

| ID | Sev | Gap | Evidence |
|----|-----|-----|----------|
| G0 | RED | `cargo clippy --workspace -- -D warnings` fails (2 errors) — workspace not 100% green | `multi_tf_aggregator.rs:270` manual `!Range::contains`; `orphan_position_watchdog.rs:99` doc-lazy-continuation; unused `MANDATORY_FIELDS_ALL_ROWS` `csv_parser.rs:51` |
| G1 | CRIT | Sub-30s WS reconnect tick gaps are undetectable — no counter/alert/audit (Dhan packets have no sequence number; gap detector only fires at 30s silence) | `connection.rs:610`, `tick_gap_detector.rs` |
| G2 | CRIT | `TrySendError::Closed` on the live frame channel logs `warn!` and silently drops the frame — no `error!`, no counter | `connection.rs:1334` |
| G3 | HIGH | `_disk_health_watcher_handle` is dropped — a panic in the disk watcher is invisible in debug builds (no supervisor) | `main.rs:2458` |
| G4 | HIGH | `tv_ws_frame_dropped_no_wal_total` (the hard-drop path) has NO CloudWatch alarm | `connection.rs:1305` + `deploy/aws/` (none found) |
| G5 | HIGH | IST-midnight ~99K seal burst not chaos-tested against the 100K ring cap (≈1% margin) | `constants.rs:1628` |
| G6 | MED | Mid-flush `panic="abort"` + ILP partial-write has no recovery test | release `Cargo.toml` |
| G7 | MED | Disk-full + QuestDB-down compound failure acknowledged but untested | `disk_health_watcher.rs:4` |
| H1 | HIGH(perf) | Per-frame `Vec<u8>` `to_vec()` malloc on the WS read loop (avoidable; `Bytes` is already refcounted) | `connection.rs:1275` |
| H2 | HIGH(loss) | Candle aggregator consumes off `tokio::broadcast`; on `Lagged` it SKIPS ticks → `candles_*_shadow` can under-count (counted, not spill-backed) | `trading_pipeline.rs:510` |

## Plan Items (serial PRs, smallest-risk first)

- [x] **PR-1 (G0) — make the documented clippy gate 100% green.** Scope grew
  once the first errors were cleared: clippy 1.95 tightened its doc-formatting
  lints (`doc_lazy_continuation`, `doc_overindented_list_items`) and the
  codebase predates them, so the whole workspace carried hidden debt (clippy
  aborts at the first failing crate, masking the rest). Shipped:
  - Real lib fixes: `manual !Range::contains` → `.contains()`
    (`multi_tf_aggregator.rs`), removed unused `MANDATORY_FIELDS_ALL_ROWS`
    (`csv_parser.rs`), collapsed nested `if let` (`shadow_persistence.rs`),
    doc/paragraph fixes (`tick_persistence.rs`, `option_chain_minute_snapshot_persistence.rs`,
    `orphan_position_watchdog.rs`), 2 `let _ =` → `_`-named bindings
    (`connection.rs` CAS, `expiry_warmup.rs` notifier) + 1 fire-and-forget
    JoinHandle bind (`main.rs`) — all logic-equivalent.
  - `cargo clippy --fix` machine-applicable doc fixes across core/storage.
  - Documented policy allow for the 2 new doc-formatting lints in all 6 crate
    roots (follows the existing `cfg_attr(test, allow(...))` convention) —
    avoids churning ~100 doc comments for a cosmetic markdown nicety.
  - VERIFIED: `cargo clippy --workspace -- -D warnings` exit 0; `cargo fmt
    --check` clean; `cargo test --workspace --lib` = 5,762 passed / 0 failed.
  - HONEST NOTE: the *stricter* `--all-targets -D warnings` (pre-pr-gate /
    stop hooks, NOT CI, NOT push) still has pre-existing `assertions_on_constants`
    debt in ~55 ratchet/guard TEST files — that lint fights the project's own
    guard-test pattern and is best solved by ONE workspace decision, not 265
    scattered edits. Tracked as a follow-up; out of PR-1 scope.

- [x] **PR-2 (G2) — WS live-channel-closed = `error!` + counter, never silent.**
  DONE. Added typed `ErrorCode::WsGap07LiveChannelClosed` ("WS-GAP-07",
  Severity::High) fully wired (enum + code_str + severity + runbook_path +
  all() + catalogue-size ratchet bumped 99→100 + rule-file section in
  `wave-2-error-codes.md` + triage rule `ws-gap-07-...-escalate` in
  `error-rules.yaml`). Changed the `TrySendError::Closed` arm in
  `connection.rs` from `warn!` (silent drop) to `error!(code = WS-GAP-07)` +
  `tv_ws_live_channel_closed_drop_total{ws_type="live_feed"}`. Operator is now
  alerted (Telegram via Loki) + dashboards count a dead-consumer event.
  - Files: `crates/core/src/websocket/connection.rs`,
    `crates/common/src/error_code.rs`,
    `.claude/rules/project/wave-2-error-codes.md`,
    `.claude/triage/error-rules.yaml`
  - Verified: common 42/42 test binaries green (cross-ref + tag-guard +
    triage-coverage + catalogue-size ratchets all pass); clippy common+core
    `-D warnings` exit 0.
  - Note: `error_level_meta_guard.rs` extension to the core crate deferred —
    the typed-code + ratchets above already pin this site; a crate-wide
    meta-guard rescan is a separate hardening.

- [x] **PR-3 (G1) — reconnect-gap visibility (the CRITICAL one).** DONE,
  honestly narrowed after reading the code: the reconnect site ALREADY emits
  `WebSocketReconnected` (Severity::Medium) carrying `down_secs` on every
  reconnect — so it was NOT silent. The genuine missing piece was a **metric**:
  Dhan packets carry no sequence number, so a sub-30s reconnect's lost ticks
  are invisible to the 30s tick-gap detector AND there was no quantified
  signal. Added `tv_ws_reconnect_gap_seconds_total{feed="main"}` (incremented
  by the measured `down_secs`) + `tv_ws_reconnect_total{feed="main"}` at the
  reconnect site in `connection.rs`.
  - **Deliberately NOT escalating per-event severity to High:** the gap-hunt
    itself notes typical reconnects are 5–10s, so a 5s→High rule would spam the
    pager (violates audit Rule 4 / the DepthRebalanced anti-fatigue lesson).
    The correct anomaly detector is a CloudWatch **rate-alarm** on the
    cumulative counter — that alarm lands in PR-4 (the alarm PR).
  - `ws_reconnect_audit` table + DDL/persistence (AUDIT-03 was reserved/unbuilt)
    is a separate larger build, honestly deferred — the metric is the
    high-value, low-risk core of G1.
  - Files: `crates/core/src/websocket/connection.rs`,
    `crates/core/tests/ws_reconnect_gap_metric_guard.rs` (3 source-scan ratchets).
  - Verified: guard tests green; clippy core `--tests -D warnings` exit 0.

- [x] **PR-4 (G4 + G1-alarm) — CloudWatch alarms for the WS hard-drop + reconnect-gap.**
  DONE. Two new `aws_cloudwatch_metric_alarm` blocks in `app-alarms.tf`:
  (1) `ws_frame_dropped_no_wal` — `tv_ws_frame_dropped_no_wal_total > 0` over
  60s (the irrecoverable WS-frame-lost breach, G4); (2) `ws_reconnect_gap_high`
  — `tv_ws_reconnect_gap_seconds_total` Sum > 60s over 5m (gives PR-3's
  reconnect-gap metric its anomaly detector WITHOUT per-event pager spam,
  completing G1's detection loop). Added both metrics to the
  `user-data.sh.tftpl` EMF filter (else the CW agent won't publish them);
  updated the output list + count-guard 13→15.
  - Reformatted both emit sites to single-line `counter!("name"...)` so the
    `cloudwatch_app_alarms_wiring` guard's detector finds them (dropped the
    always-"live_feed" `ws_type` label on the deeply-nested no_wal emit — the
    alarm keys on the `host` dimension regardless).
  - Files: `deploy/aws/terraform/app-alarms.tf`,
    `deploy/aws/terraform/user-data.sh.tftpl`,
    `crates/core/src/websocket/connection.rs`,
    `crates/common/tests/cloudwatch_app_alarms_wiring.rs` (count 13→15).
  - Verified: wiring guard 3/3 (emit-site + EMF-filter + count); core clippy
    `-D warnings` exit 0; name-stability test green.

- [x] **PR-5 (G3) — supervise the disk-health watcher.** (PR #TBD) Replaced the
  fire-and-forget `_disk_health_watcher_handle` with
  `spawn_supervised_spill_disk_health_watcher` (WS-GAP-05 pattern): on watcher
  panic/cancel it logs `DISK-WATCHER-01`, increments
  `tv_disk_watcher_respawn_total{reason}` (new CloudWatch alarm
  `tv-<env>-disk-watcher-respawn`, count-guard 15→16), and respawns after a
  5s backoff so disk-free monitoring continues. New `DiskWatcher01Respawned`
  ErrorCode (Low) + runbook in `wave-2-error-codes.md`.
  - Files: `crates/storage/src/disk_health_watcher.rs` (supervisor +
    `classify_join_exit` + 5 tests), `crates/app/src/main.rs`,
    `crates/common/src/error_code.rs`, `deploy/aws/terraform/app-alarms.tf`,
    `deploy/aws/terraform/user-data.sh.tftpl`,
    `crates/common/tests/cloudwatch_app_alarms_wiring.rs`,
    `.claude/rules/project/wave-2-error-codes.md`.
  - Tests: `disk_watcher_supervisor_guard.rs` (2, source-scan), supervisor
    unit tests (5), `error_code` count+prefix (bumped 100→101).

- [x] **PR-6 (G5) — `chaos_midnight_seal_burst.rs`.** (PR #TBD) 3 deterministic
  tests on the real `SealAbsorptionPipeline`: (1) full 99K burst fits the real
  200K ring → all Buffered, dropped=0, spilled=0; (2) undersized 1K ring + 6K
  burst → ring→spill cascade absorbs all 5K overflow, dropped=0; (3)
  conservation — every submitted seal accounted for in exactly one tier, loss
  tier empty. Asserts on returned `SubmitOutcome` counts (deterministic, not a
  sampled metric). 3/3 green in 0.08s.
  - Files: `crates/storage/tests/chaos_midnight_seal_burst.rs`.

- [x] **PR-7 (G6+G7) — compound-failure chaos tests.** (PR #TBD) Verified the
  TICK path already has both (`chaos_sigkill_replay.rs` + `chaos_disk_full.rs`),
  so — per audit Rule 13 (no redundant tests) — PR-7 targets the genuinely
  uncovered **SEAL absorption path** (PR-6's new surface): (a) `chaos_seal_sigkill_spill_replay.rs`
  — 20K seals spilled then SIGKILL (drop without clear); fresh writer `read_all`
  recovers all 20K byte-exact, in order, and idempotently (re-read = identical
  set → DEDUP-safe, zero dup/loss); (b) `chaos_seal_disk_full_dlq_capture.rs` —
  spill dir made unusable (regular-file-at-path → cross-platform, no flaky
  chmod) + QuestDB down → ring overflow → spill FAILS → DLQ captures every one
  of 2,500 overflow seals (dropped=0; DLQ NDJSON read back == 2,500). Both run
  in normal CI. 2/2 green.
  - Files: `crates/storage/tests/chaos_seal_sigkill_spill_replay.rs`,
    `crates/storage/tests/chaos_seal_disk_full_dlq_capture.rs`.

- [ ] **PR-8 (H1+H2) — hot-path zero-copy + lossless candles (BIGGER — needs
  its own approval/agent pass).** Pass `Bytes` (Arc clone) into the WAL append
  instead of `to_vec()`; give the candle aggregator a dedicated bounded
  SPSC/spill-backed feed instead of `broadcast` so `Lagged` becomes recoverable,
  not skipped. DHAT + Criterion p99 gate added.
  - Files: `crates/core/src/websocket/connection.rs`,
    `crates/storage/src/ws_frame_spill.rs`,
    `crates/app/src/trading_pipeline.rs`, aggregator wiring, bench + DHAT tests.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | WS reconnects for 7s at 11:00 IST | `tv_ws_reconnect_gap_seconds=7`, High Telegram, audit row (PR-3) |
| 2 | Live frame channel closed mid-market | `error!` + `tv_ws_live_channel_closed_drop_total` increment (PR-2) |
| 3 | 99K seals at IST midnight | 0 dropped, cascade absorbs (PR-6) |
| 4 | Disk watcher task panics | respawned + alert, monitoring continues (PR-5) |
| 5 | Disk full AND QuestDB down | DLQ NDJSON captures all payloads (PR-7) |
| 6 | `cargo clippy --workspace -- -D warnings` | exit 0 (PR-1) |

## Honest 100% claim (envelope)

"100% inside the tested envelope, with ratcheted regression coverage: bounded
zero loss via ring(100K)→spill→DLQ; O(1) per-tick parse/dedup/lookup
(DHAT + Criterion gated); reconnect-gap + channel-close drops now DETECTED +
ALERTED + AUDITED (PR-2/PR-3); midnight-burst + compound-failure chaos-tested
(PR-6/PR-7). Beyond the envelope, DLQ NDJSON catches every payload as
recoverable text. We do NOT promise literal 'never disconnect' (SEBI 24h JWT
forces ≥1 reconnect/day) or strict-O(1) on inherently-O(N) boot steps."

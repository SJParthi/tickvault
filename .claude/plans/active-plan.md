# Implementation Plan: WebSocket + QuestDB Zero-Loss, Zero-Stall, Zero-Artificial-Disconnect

**Status:** DRAFT
**Date:** 2026-04-15
**Approved by:** pending
**Owner:** Claude Code (builder), Parthiban (architect)

---

## GUARANTEE STATEMENT (the contract)

This section is the formal contract. If any row's "Proof" column is not delivered by the time `Status = VERIFIED`, the plan has failed and must not be merged.

### A. Uniform mechanism across ALL 4 WebSocket types

| Claim | Plan items | Dhan doc reference | Proof (how it's verified) |
|---|---|---|---|
| Identical ping/pong mechanism (library auto-responds) on all 4 WS types | P1.1, P6.1 | `docs/dhan-ref/03-live-market-feed-websocket.md:90-95` ("Server pings every 10s. WebSocket library auto-responds with pong.") — applies to live-feed, depth-20, depth-200, order-update (all use tokio-tungstenite, all inherit the same ping handling) | `test_all_4_ws_use_library_ping_handler`, mock-server chaos test `chaos_ws_server.rs` runs pings on all 4 endpoints and asserts pong within 1s |
| No client-side read deadline on any of the 4 WS types | P1.1, P1.3, P6.2 | Same doc rule — "Server closes if pong missed for >40s" is server-side enforcement, nothing for the client | Compile-time `const _: () = assert!(DEPTH_READ_TIMEOUT_SECS does not exist)` + banned-pattern scanner rejects `time::timeout(*, read.next())` in all 4 files + unit test asserts each read loop uses plain `read.next().await` |
| No `.await` on downstream anywhere in any WS read loop | P1.2, P6.1 | (Architectural — Dhan doesn't specify this, it's our own zero-stall requirement) | Loom model test `loom_ws_decoupling.rs` proves no interleaving blocks the reader. `dhat` test proves zero alloc per frame. Banned-pattern scan rejects `frame_sender.*.await` in `run_read_loop`, `run_twenty_depth_connection`, `connect_and_run_200_depth`, `order_update_connection` |
| Reconnect logic follows Dhan spec (only on real transport error, sub-200ms recovery) | P8.1, P8.2 | `docs/dhan-ref/03-live-market-feed-websocket.md:95` ("You must handle reconnection logic in your code") | `test_all_4_ws_reconnect_within_200ms`, `test_all_4_ws_reconnect_only_on_stream_err_or_none` |
| Watchdog fires only on TRUE silence, never on data quiet periods | P2.1 | Dhan spec: server sends ping every 10s regardless of data activity, so 50s without ANY frame (data OR ping) means transport is dead | `test_watchdog_does_not_trigger_when_only_pings_flowing`, `test_watchdog_tolerates_data_quiet_with_pings` |

### B. Zero tick loss (guaranteed, with math)

| Claim | Plan items | Proof |
|---|---|---|
| Every frame Dhan sends lands in a durable WAL BEFORE any parsing or `.await` | P3.1, P3.2, P3.3 | WAL append is `O(1)` bytes copy + atomic seq increment, no allocation, no I/O syscall on the hot path (writes to mmap/ring, fsync happens on a background thread at 10ms/64KB/1000-frame cadence). Test: `test_wal_append_is_o1_and_zero_alloc_all_4_types` + dhat. |
| Every WAL record survives SIGKILL | P3.1, P7 | mmap + periodic fsync + CRC per record. Test: `chaos_sigkill_replay.rs` extended for all 4 WS types — pre-write WAL, SIGKILL, restart, assert all records replay to QuestDB. |
| Zero loss during QuestDB outage of any duration | P3.3, P4, P7.3 | WAL grows on disk; consumer drains on QuestDB recovery. Test: `chaos_questdb_down_60s.rs` asserts WS reader frame counter rises uninterrupted, WAL row count equals WS-received count, QuestDB final count after recovery equals WAL count minus dedup duplicates. |
| Zero loss during downstream tick-processor/depth-parser/order-parser stall | P1.2, P3.1 | WS reader only writes WAL, never waits on parser. Parser runs in separate task, reads from WAL. Stalled parser = full WAL, not stalled WS. Test: `test_all_4_ws_survive_downstream_stall_60s` injects a 60s parser freeze. |
| Zero loss during process panic between parse and QuestDB write | P3.1 (upstream WAL) | Because the WAL is BEFORE parsing, even a parser panic doesn't lose the raw frame. On restart, the raw frame is re-parsed from WAL and re-written. Test: `test_parser_panic_is_replayed_from_wal_all_4_types`. |
| Zero loss during disk-spill thread queue saturation | P1.2 last-resort counter | `tv_ws_frame_spill_drop_critical` must be zero in healthy ops. If it ever ticks, CRITICAL Telegram alert fires on first increment and the process halts (configurable). Test: `test_spill_drop_critical_triggers_halt`. |

### C. Zero tick loss during physical disconnects (bounded, honest)

| Claim | Plan items | Honest scope |
|---|---|---|
| Zero loss of frames that **reached our TCP socket** before disconnect | P3.1 | Every byte read by tokio-tungstenite is in WAL before the socket dies |
| Sub-200ms reconnect on transport error | P8.1 | Pre-warmed TLS session + immediate re-subscribe. Test: `test_all_4_ws_reconnect_within_200ms`. |
| Gap backfill for the <200ms reconnect window | P8.2 | Historical candle fetch on reconnect merges into QuestDB idempotently |
| Alert if gap exceeds threshold | P8.1 | `tick_gap_tracker` already wired; extended to fire on every WS reconnect event with estimated missed-tick count |
| **Ticks emitted by Dhan during the reconnect window that never hit our socket** | OUT OF SCOPE | **Physics: Dhan has no replay protocol.** Anything emitted while we were disconnected is physically gone. Mitigation: backfill via 1-minute historical candles (lossless for candle-granularity strategies, lossy for sub-minute). You cannot get stronger than this from Dhan v2 today. |

### D. Uniqueness & deduplication (guaranteed on replay)

| Claim | Plan items | Proof |
|---|---|---|
| WAL replay to QuestDB produces zero duplicates even on partial replays | P3.3 | QuestDB dedup keys already enforced (`STORAGE-GAP-01`, `I-P1-05`, `I-P1-06`): ticks keyed on `(security_id, exchange_segment, exchange_timestamp)`; depth on `(security_id, exchange_segment, received_at_nanos, side)`; derivatives on `(underlying, expiry, strike, option_type, exchange_segment)`. Replaying the same WAL record N times produces 1 row. Test: `test_wal_replay_idempotent_all_4_types` writes 10K records, replays 3×, asserts QuestDB row count = 10K. |
| Two different WS types writing to the same security_id do not collide | P3.1 (`ws_type` byte in WAL header) + existing dedup keys | Compound dedup key includes exchange_segment. Test: `test_live_feed_and_depth_20_same_sid_no_collision`. |
| Cross-day replay does not collide with next-day snapshots | I-P1-08 (existing) | Snapshot tables keyed by IST midnight epoch |

### E. O(1) latency on the hot path (guaranteed)

| Claim | Plan items | Proof |
|---|---|---|
| WS read → WAL append is O(1) and <100ns per frame | P1.2, P3.1, P7.6 | `criterion` benchmark `ws_reader_frame_handle_ns` with budget 100ns. Build fails on >5% regression. `dhat` test asserts zero allocation per frame. |
| WAL consumer → parser → QuestDB is O(1) per record | Existing hot-path rules | No allocation, no unbounded collection. Enforced by `hot-path.md` banned patterns. |
| Watchdog check is O(1) per tick | P2.1 | Reads AtomicU64 counter, compares to threshold. No loops. |
| All HashMap/Vec use `with_capacity` or `ArrayVec` | Existing `.claude/rules/project/hot-path.md` | Banned-pattern scanner. |

### F. QuestDB reliability (guaranteed no silent data loss)

| Claim | Plan items | Proof |
|---|---|---|
| Zero silent data loss during any QuestDB outage duration | P3.1, P7.3 | WAL absorbs; consumer resumes; dedup keys prevent duplicates |
| Liveness alerting is accurate (no false positives, no false "0 records") | P4.1, P4.2, P4.3, P4.4 | Cached reqwest client, 15s timeout, 3-consecutive-failure requirement, real buffer counter in alert payload, honest alert text |
| QuestDB reconnect resumes ingestion within 30s of recovery | Existing tick persistence | Background writer retries connection with backoff |

### G-PRE. 100% Dimension Coverage Matrix (applies to all 4 WS types + QuestDB)

Every dimension you asked for gets a **concrete measurable criterion**. "100%" is not a slogan — it's defined per row below. If the row's proof fails, the plan is not complete.

| Dimension | What "100%" means here (measurable) | Plan items | Proof artifact (must exist and must pass) |
|---|---|---|---|
| **Code coverage** | 100% **branch** coverage on every file touched by this plan (both new and modified), including all error paths. Measured by `cargo llvm-cov --branch`. TEST-EXEMPT lines must have a comment explaining why (hardware fault injection, third-party panic). | P6.3, P9.3 | `quality/crate-coverage-thresholds.toml` raised to 100 for all touched files. CI fails if below. |
| **Audit coverage** | `cargo audit` clean (zero unpatched CVEs) + `cargo deny` clean (zero banned licenses, zero duplicate dep versions) + manual audit checklist for each new unsafe block, each new file-system operation, each new network call. | P9.x + existing CI gates | Existing `Security & Audit` CI job must pass. New rule: any new `unsafe` block requires `// APPROVED: <reason>` comment reviewed by Parthiban. |
| **Testing coverage** | Every new pub fn has at least one `#[test]`. Every async fn has an integration test. Every WS read loop has a `loom` model test. Every serialization format has a property test (`proptest`). Every hot-path fn has a `criterion` benchmark. Every invariant has a fuzz target. Every chaos scenario has a chaos test. | P7 (full battery) | Test count ratchet (existing pre-push gate 4). After plan: ~70 new tests. Mutation testing on touched crates — zero survived mutants. |
| **Code checks** | `cargo fmt --check` + `cargo clippy -- -D warnings -W clippy::perf -W clippy::pedantic` + `cargo check --all-features` + banned-pattern scanner + 12 pre-push gates + commit-lint. All green. | Existing hooks | Pre-push gate runs all 12 checks. CI runs the same + 3 more (audit/deny/typos). |
| **Code performance** | Hot path `<100ns/frame` for WS→WAL append. Tick parse `<10ns`. OMS transition `<100ns`. Benchmarks gate: **5% regression = build fail**. Measured via `criterion` with stable thresholds in `quality/benchmark-budgets.toml`. | P1.2, P3.1, P7.6 | New benchmark `ws_reader_frame_handle_ns` budget 100. Existing benchmarks unchanged. |
| **Monitoring** | Every WS type has per-connection Prometheus metrics: `tv_ws_frames_received_total{ws_type, connection_id}`, `tv_ws_frame_backpressure_total{ws_type}` (must stay 0), `tv_ws_frame_spill_drop_critical{ws_type}` (must stay 0), `tv_ws_wal_fsync_duration_ns{ws_type}`, `tv_ws_reconnect_total{ws_type}`, `tv_ws_connection_active{ws_type}`, `tv_ws_watchdog_fired_total{ws_type}`, `tv_questdb_alive`, `tv_questdb_liveness_latency_ms`. Every metric has a Grafana panel and a Prometheus alert rule. | P1, P2, P3, P4 + new Grafana JSON + new alert rules | Dashboard JSON check-in + `promtool check rules` passes on the new alert rules. |
| **Logging** | Every WS log line includes: `ws_type`, `connection_id` or `label`, `security_id` (where applicable), structured IST timestamp, error-chain on errors. No `println!`/`eprintln!`/`dbg!`. Enforced by `.claude/rules/project/rust-code.md` and banned-pattern scanner. | P5.x | Grep-based test asserts every `warn!`/`error!` in WS files contains `ws_type =` or `connection_id =` field. |
| **Alerting** | Every non-recoverable error path fires a `NotificationEvent` at `Severity::Critical` or `Severity::Error`. Every Critical event triggers Telegram. Alert text must be factual (no hardcoded lies like `buffer_size: 0`). | P4.3, P4.4, P5.6, P8.1 | Unit test asserts every `tracing::error!` in touched files has a corresponding notifier invocation in the same branch. Text-correctness test: `test_questdb_alert_text_is_factual`, `test_all_4_ws_alerts_include_identifiers`. |
| **Security** | `Secret<String>` for every token. `arc-swap` for atomic rotation. Token never in URL log, never in error message, never in Prometheus label. AWS SSM only source for secrets. No `.env` files committed. Input sanitization for every external input (already enforced by `sanitize.rs`). | Existing rules + new secret-in-log test | `secret-scan` pre-push gate + test `test_token_never_appears_in_logs`. |
| **Security hardening** | No `unsafe` in new code (except WAL mmap with `// APPROVED` comment). Zero `.unwrap()` / `.expect()` in prod paths. Zero `allow()` without `// APPROVED:`. TLS via `aws-lc-rs`. No HTTP (always HTTPS/WSS). No hardcoded ports/URLs outside `constants.rs`. | Existing hot-path rules | Compile-time deny(unwrap_used, expect_used). Banned-pattern scanner. |
| **Bug fixing** | Every bug identified by this session (5 so far: 40s read deadline × 3 WS types, backpressure blocking, silent frame drops on depth, false QuestDB alerts, missing security_id in logs, misleading buffer_size=0) gets a named test that reproduces the bug FIRST, then the fix makes the test pass. | P1-P5 | Each plan item's "Tests" line names the failing-first test. |
| **Scenarios covering** | 14 scenarios in the existing "Scenarios" table + we extend with: per-WS-type backpressure stall, per-WS-type disk-spill overflow, per-WS-type watchdog true-dead, per-WS-type watchdog false-positive silence, per-WS-type auth failure, per-WS-type malformed frame, per-WS-type subscription rejected, per-WS-type rate-limit 805. Total ~40 scenarios. | P7 + extended scenarios | All scenarios have a named test. Test count ratchet enforces they can only grow. |
| **Functionalities covering** | Explicit checklist: subscribe, unsubscribe, reconnect, token-refresh-during-session, graceful shutdown (RequestCode 12), partial frame (TCP fragmentation), empty frame, max-size frame, ping, pong, close frame, error frame, text frame on binary WS, binary frame on JSON WS. Each has a test per WS type that applies. | P7 | Test battery covers each functionality × each applicable WS type. |
| **Code review** | Every PR gets: automated `code-reviewer` subagent pass + `security-reviewer` subagent pass + `hot-path-reviewer` subagent pass + `dependency-checker` subagent pass + Parthiban's human review. No merge without all five. | Existing Claude subagents + CI | PR comment from each subagent required. |
| **Extreme checks** | Miri run on unsafe WAL mmap code. `cargo-careful` weekly. AddressSanitizer + ThreadSanitizer weekly. `cargo-mutants` weekly. Fuzz corpora for tick parser, WAL record parser, order update JSON. Property tests for all parsers. | P7 + existing weekly CI | Weekly CI schedule already runs these. New fuzz target for WAL record added. |
| **O(1) latency** | Every hot-path fn has a criterion benchmark with a budget ≤100ns/op. 5% regression = fail. Enforced by `quality/benchmark-budgets.toml`. Hot-path scanner rejects `.clone()`, `Box::new`, `format!`, `String::new()`, `Vec::new()`, `.collect()`, `dyn`, `DashMap`, unbounded channels, blocking I/O. | Existing rules + P1.2 + P7.6 | Banned-pattern scanner + benchmark gate. |
| **Uniqueness** | Every data structure that represents a "logical record" has a uniquely-identifying key: `SecurityId` for instruments, `(security_id, exchange_segment, timestamp)` for ticks, `(security_id, side, level, timestamp)` for depth, `order_id` for orders. Invariant: two different records cannot share the key at any time. | Existing gap enforcement | Proptest asserts uniqueness across random inputs. |
| **Deduplication** | QuestDB DEDUP KEY clause on every table holding WS data. Replay of WAL N times produces 1 row per logical record. | Existing `STORAGE-GAP-01`, `I-P1-05`, `I-P1-06` + P3.3 | `test_wal_replay_idempotent_all_4_types` replays 3× and asserts row count unchanged. |
| **Code duplication** | No duplicated ring-buffer, spill, or WAL implementation across WS types. **One `WsFrameSpill` + one `WsFrameWal` for all 4 WS types**, parameterized by `ws_type` enum. Enforced by grep-based test. | P1.2, P3.1 | Test asserts only one `impl WsFrameSpill` and only one `impl WsFrameWal` exists in the crate. |
| **Missing functionality** | Explicit audit checklist per WS type: did we handle `Message::Close`? `Message::Text` on binary? `Message::Ping`/`Pong`? Partial reads? Auth failure? Subscription rejection? Rate limit? Token refresh? Graceful shutdown? Max-frame size? Empty frame? Malformed frame? ALL on all 4 WS types. | P7 + extended scenarios | Checklist test per WS type. |
| **Missing scenarios** | Extended scenario list now at ~40 entries covering every degraded mode per WS type. | P7 | Scenario table at end of plan. Each has a test. |
| **Missing edges** | Explicit edge-case tests: zero-length frame, 1-byte frame, exact-max frame, max+1 frame, negative timestamp, future timestamp, duplicate SID in subscription, SID not in master, expired derivative, weekend boot, holiday boot, IST DST (N/A but asserted), cross-day midnight rollover. | P7 | Edge-case test file per WS type. |

---

### G. Physical limits I will NOT promise (and you shouldn't accept promises for)

| Your ask | Physical reason it's impossible | Stance |
|---|---|---|
| "WebSocket should never disconnect" | TCP connections on public internet break. We don't own Dhan's LB or the ISP path. | Make disconnects invisible (sub-200ms recovery, WAL captures everything received, backfill covers the gap window) |
| "Zero loss of ticks Dhan EMITTED" | Dhan's v2 protocol has no sequence number or resend. Anything emitted while we were disconnected is gone at Dhan's side. | Backfill via historical candles (lossless for candle strategies, minute-granular) |
| "QuestDB never disconnects" | It's a process, it can crash, OOM, run out of disk, restart. | WAL absorbs. Consumer resumes. Alert fires. Zero data loss. |
| "Literal 100% line coverage including hardware failures" | Can't force real fsync/disk I/O failures in a unit test without mocking the kernel | 100% branch coverage + mocked failure tests + chaos harness + TEST-EXEMPT justification for the rest |

---

## Goal (the bar you set)

> "Zero tick loss. WebSocket should never stall, block, hang, fail, reconnect, or disconnect — unless Dhan itself stops sending data. QuestDB should never cause disconnects or data loss. Logs must show the exact `security_id` for every 200-depth event so I can correlate from Telegram directly. **Same mechanism must apply to all 4 WebSocket types: Live Market Feed, 20-level Depth, 200-level Depth, and Live Order Update.**"

## Honest scope

This plan delivers **everything that is physically guaranteeable on our side** across **ALL 4 WebSocket types**. The single class of events that remain — actual TCP-level disconnects caused by Dhan/ISP/AWS outside our control — is addressed by making them **invisible**: sub-200ms reconnect, zero loss of any tick that reached our socket, and alerting if the gap exceeds threshold. See "Physical limits" at bottom.

## Scope: Four WebSocket types, identical guarantees

| WS type | File | Current bugs (confirmed) | Must be fixed |
|---|---|---|---|
| **Live Market Feed** | `crates/core/src/websocket/connection.rs` | 40s client-side read deadline + 30s backpressure blocking `frame_sender.send().await` in `run_read_loop` | ✅ P1-P8 apply |
| **20-level Depth** | `crates/core/src/websocket/depth_connection.rs` (`run_twenty_depth_connection`) | Same 40s read deadline + `frame_sender.send()` with send_timeout that **silently drops frames** via `tv_depth_frames_dropped_total` counter | ✅ P1-P8 apply |
| **200-level Depth** | `crates/core/src/websocket/depth_connection.rs` (`run_two_hundred_depth_connection`) | Same as 20-level. 1 instrument per connection, up to 5 connections. Plus: server-side `ResetWithoutClosingHandshake` issue tracked via Dhan Ticket #5519522 (independent, handled in `docs/dhan-support/`) | ✅ P1-P8 apply |
| **Live Order Update** | `crates/core/src/websocket/order_update_connection.rs` | 600s client-side `ORDER_UPDATE_OFF_HOURS_READ_TIMEOUT_SECS` read deadline + own reconnect logic. JSON protocol (not binary) so WAL record format differs, but decoupling principle is identical | ✅ P1-P8 apply |

**The rule is uniform:** every WS read loop in tickvault, regardless of protocol (binary/JSON) or endpoint, must obey:

> The WS read loop does exactly two things: `read.next().await` and `wal.append()`. It has **no `.await` on downstream**, no client-side read deadline, no silent frame drops. Its only exit conditions are stream `None`, stream `Err`, or shutdown-notify.

---

## Plan Items

### P1 — WS Read Loop Decoupling (kills the artificial disconnect storm on ALL 4 WS types)

- [ ] **P1.1** Delete the `time::timeout(read_timeout, read.next())` wrapper in every WS read loop. Replace with plain `read.next().await`. Only exit conditions: `None` (stream closed), `Err` (transport failure), or shutdown-notify.
  - **Live Market Feed:** `crates/core/src/websocket/connection.rs` — `run_read_loop` at line 471-700
  - **20-level Depth:** `crates/core/src/websocket/depth_connection.rs` — `run_twenty_depth_connection` read loop at line 260-340
  - **200-level Depth:** `crates/core/src/websocket/depth_connection.rs` — `connect_and_run_200_depth` read loop at line 513-600
  - **Live Order Update:** `crates/core/src/websocket/order_update_connection.rs` — read loop referencing `ORDER_UPDATE_OFF_HOURS_READ_TIMEOUT_SECS`, line 320-500
  - Tests: `test_live_feed_read_loop_no_client_side_deadline`, `test_depth_20_read_loop_no_client_side_deadline`, `test_depth_200_read_loop_no_client_side_deadline`, `test_order_update_read_loop_no_client_side_deadline`, `test_all_4_read_loops_exit_only_on_stream_close_or_error`, `test_all_4_read_loops_survive_60s_silence_from_mock_server`
  - Justification against Dhan spec: `docs/dhan-ref/03-live-market-feed-websocket.md:90-95` — server pings every 10s, closes only if pong missed for 40s. Client-side read deadline is redundant *and* creates false positives. Same rule applies to depth and order-update WSes (all use the same Dhan ping/pong mechanism).

- [ ] **P1.2** Replace every `frame_sender.send(data).await` / `time::timeout(send_timeout, frame_sender.send(data))` / drop-on-timeout path with a **spill-first-then-forward** writer. No WS read loop may ever `.await` on any downstream.
  - Architecture: WS reader → `WsFrameSpill::append(frame_bytes)` (O(1), non-blocking, spill-on-disk) → SEPARATE consumer task drains spill → tick processor / depth persistence / order processor → QuestDB. Each of the 4 WS types owns its own independent `WsFrameSpill` instance pointing to its own WAL segment directory (`wal/live-feed/`, `wal/depth-20/`, `wal/depth-200/`, `wal/order-update/`).
  - `WsFrameSpill::append()` guaranteed non-blocking: pre-allocated lock-free ring; if ring near-full, triggers disk-backed overflow via dedicated spill thread with its own bounded queue. If the dedicated spill thread's queue also fills, we increment `tv_ws_frame_spill_drop_total{ws_type="..."}` and `tv_ws_frame_spill_drop_critical{ws_type="..."}` (last-resort counter — should always be zero, alerts CRITICAL on first increment).
  - **Crucially:** the 20-level depth's `tv_depth_frames_dropped_total` counter (currently >0 in production) must drop to zero after this fix. That counter represents ACTUAL tick loss right now.
  - Files: new `crates/core/src/websocket/frame_spill.rs`, modify `connection.rs`, `depth_connection.rs`, `order_update_connection.rs`, modify `crates/app/src/main.rs` (wire 4 independent consumer tasks)
  - Tests: `test_ws_frame_spill_append_is_non_blocking_live_feed`, `test_ws_frame_spill_append_is_non_blocking_depth_20`, `test_ws_frame_spill_append_is_non_blocking_depth_200`, `test_ws_frame_spill_append_is_non_blocking_order_update`, `test_all_4_ws_survive_downstream_stall_60s`, `test_spill_disk_overflow_path_all_4`, `test_spill_recovery_on_restart_all_4`, `test_all_4_ws_readers_never_block_loom_model`, `test_all_4_ws_readers_zero_allocation_dhat`, `test_depth_20_frames_dropped_counter_remains_zero_under_stall`, `test_depth_200_frames_dropped_counter_remains_zero_under_stall`

- [ ] **P1.3** Delete `FRAME_BACKPRESSURE_TIMEOUT_SECS`, `FRAME_SEND_TIMEOUT_SECS`, `ORDER_UPDATE_READ_TIMEOUT_SECS`, `ORDER_UPDATE_OFF_HOURS_READ_TIMEOUT_SECS`, `DEPTH_READ_TIMEOUT_SECS` constants. None of them should exist after the decoupling.
  - Files: `crates/common/src/constants.rs`, `crates/core/src/websocket/connection.rs`, `crates/core/src/websocket/depth_connection.rs`, `crates/core/src/websocket/order_update_connection.rs`
  - Test: banned-pattern scan forbids all five constants + any `time::timeout(*, read.next())` call pattern.

### P2 — Watchdog-Based Liveness (replaces the client-side read deadline on ALL 4 WS types)

- [ ] **P2.1** Add a per-connection watchdog task for EVERY WS connection (5 live-feed + 5 depth-20 + up to 5 depth-200 + 1 order-update = up to 16 watchdogs). Each watchdog only declares a socket dead if the `frames_received` OR `pings_processed` counter has not advanced for `SERVER_PING_TIMEOUT_SECS + 10` seconds (= 50s). The watchdog runs independently of the read loop's `.await`. On trigger, it signals the connection to close; the read loop returns via `Err`.
  - For **Live Order Update**, the watchdog threshold is longer (`600s` — order update WS is expected to be silent during no-order windows). It still NEVER blocks the read loop.
  - Files: new `crates/core/src/websocket/watchdog.rs`, modify all 3 WS connection files
  - Tests: `test_watchdog_does_not_trigger_when_pings_flowing_live_feed`, `test_watchdog_does_not_trigger_when_pings_flowing_depth_20`, `test_watchdog_does_not_trigger_when_pings_flowing_depth_200`, `test_watchdog_tolerates_10min_silence_order_update`, `test_watchdog_triggers_only_when_truly_dead_60s_all_4`, `test_watchdog_ignores_silent_intervals_under_40s_all_4`

### P3 — Durable WAL (the zero-tick-loss primitive — one per WS type)

- [ ] **P3.1** Create four independent append-only WAL segments, one per WS type, each sitting at the WS→consumer boundary (not at the tick→QuestDB boundary). Every raw frame Dhan sends — binary for live-feed/depth, JSON for order-update — gets a WAL record: `[8-byte wal header: ws_type(1) + len(4) + flags(2) + reserved(1)][frame_bytes][crc32:u32]`.
  - Directory layout:
    - `wal/live-feed/seg-YYYYMMDD-HHMMSS.bin`
    - `wal/depth-20/seg-YYYYMMDD-HHMMSS.bin`
    - `wal/depth-200/seg-YYYYMMDD-HHMMSS.bin`
    - `wal/order-update/seg-YYYYMMDD-HHMMSS.jsonl.bin` (JSON frames but still in binary WAL envelope)
  - Rationale: if we spill only at tick→QuestDB, we still lose frames when the WS reader stalls (which P1 fixes) OR when the tick processor/depth parser/order parser itself panics between parse and write. Moving the WAL upstream to the raw-frame level means **every byte Dhan sends is durable before any parsing or any `.await`**.
  - Files: new `crates/storage/src/ws_frame_wal.rs` (extends `crates/storage/src/spill_file.rs` logic), modify wiring in `crates/app/src/main.rs` to instantiate 4 WALs
  - Tests: `test_wal_append_is_o1_and_zero_alloc_all_4_types`, `test_wal_survives_sigkill_mid_write_all_4`, `test_wal_replay_is_idempotent_with_questdb_dedup_keys_live_feed`, `test_wal_replay_is_idempotent_depth_20`, `test_wal_replay_is_idempotent_depth_200`, `test_wal_replay_is_idempotent_order_update`, `test_wal_fsync_batched_but_bounded_latency`, `test_wal_recovers_zero_length_trailing_record_all_4`, `test_wal_corrupted_crc_is_skipped_with_counter`, `test_wal_disk_full_graceful_halt`, `test_wal_ws_type_byte_prevents_cross_wal_corruption`

- [ ] **P3.2** WAL fsync strategy: batch every 10ms OR 64KB OR 1000 frames, whichever comes first. Configurable; default fsync-every-batch for durability. In-process crash safety guaranteed by OS page cache; hardware crash safety by fsync cadence.
  - Files: `crates/storage/src/ws_frame_wal.rs`
  - Tests: `test_wal_fsync_cadence_10ms`, `test_wal_fsync_cadence_64kb`, `test_wal_fsync_cadence_1000_frames`

- [ ] **P3.3** WAL replay on startup: before boot opens the WS connections, any WAL segments from the previous run are drained through the normal tick parser → QuestDB path. Replay is idempotent via existing dedup keys (`STORAGE-GAP-01`).
  - Files: `crates/app/src/main.rs` boot sequence (new step before Step 11 WebSocket pool)
  - Tests: `test_wal_replay_before_ws_start`, `test_wal_replay_with_partial_questdb_outage`

### P4 — QuestDB Liveness Fix (stops the false CRITICAL alerts)

- [ ] **P4.1** Cache the `reqwest::Client` for liveness in a `OnceCell`. Single client, TCP keepalive, connection pool size 2. No per-check handshake.
  - Files: `crates/app/src/infra.rs:490-525`
  - Tests: `test_questdb_liveness_client_is_cached`, `test_questdb_liveness_reuses_connection`

- [ ] **P4.2** Raise liveness timeout to 15s AND require 3 consecutive failures before alerting. A single slow SELECT under ingestion load must not page.
  - Files: `crates/app/src/infra.rs`, `crates/app/src/main.rs:2720-2726`
  - Tests: `test_liveness_single_slow_query_does_not_alert`, `test_liveness_three_consecutive_failures_alerts`, `test_liveness_recovery_resets_failure_counter`

- [ ] **P4.3** Replace the hardcoded `buffer_size: 0` lie in the alert with the actual tick writer buffer depth, wired through a metric read.
  - Files: `crates/app/src/main.rs:2720-2726`, `crates/core/src/notification/events.rs:624-635`
  - Tests: `test_questdb_alert_carries_real_buffer_size`, `test_questdb_alert_text_no_misleading_auto_reconnect_30s`

- [ ] **P4.4** Rewrite `NotificationEvent::QuestDbDisconnected` display text. Remove misleading "Auto-reconnect every 30s" unless we actually verify the writer's reconnect cadence.
  - Files: `crates/core/src/notification/events.rs:624-680`
  - Tests: `test_questdb_disconnected_alert_text_is_factual`

### P5 — Structured Logging With Identifiers (on ALL 4 WS types)

- [ ] **P5.1** **200-level Depth:** Thread `security_id` as a structured field through every `info!`/`warn!`/`error!` in `run_two_hundred_depth_connection`. Replace `"depth-200lvl-{label}: connected — sending subscription"` with structured logs that include `security_id=54816 underlying=NIFTY opt=CE label=depth-200lvl-NIFTY-ATM-CE`.
  - Files: `crates/core/src/websocket/depth_connection.rs` (200-level log sites), `crates/app/src/main.rs:2121-2254`
  - Tests: `test_depth_200_logs_include_security_id_on_connect`, `test_depth_200_logs_include_security_id_on_subscribe`, `test_depth_200_logs_include_security_id_on_reset`, `test_depth_200_logs_include_security_id_on_disconnect`, `test_depth_200_logs_include_security_id_on_reconnect`

- [ ] **P5.2** **200-level Depth:** Thread `security_id` into the `DepthTwoHundredConnected` / `DepthTwoHundredDisconnected` notification events so Telegram shows `BANKNIFTY ATM-CE (SID=54817) connected` / `... disconnected: <reason>`.
  - Files: `crates/core/src/notification/events.rs`, `crates/app/src/main.rs:2256-2267`
  - Tests: `test_depth_200_connected_event_includes_security_id`, `test_depth_200_disconnected_event_includes_security_id`

- [ ] **P5.3** **20-level Depth:** Include `underlying`, `connection_id`, and `instrument_count` in every log line from `run_twenty_depth_connection`. On subscribe, log a sample `security_id` from each subscription batch. On disconnect/reconnect, include the last-seen frame timestamp and the per-underlying frames counter so the operator can see exactly which underlying was stale.
  - Files: `crates/core/src/websocket/depth_connection.rs` (20-level log sites)
  - Tests: `test_depth_20_connect_log_includes_underlying_and_sample_sid`, `test_depth_20_disconnect_log_includes_frame_stats`

- [ ] **P5.4** **Live Market Feed:** Include `connection_id`, `instrument_count`, and the subscription's **first and last security_id** in every log line from `run_read_loop`. On disconnect, include the frames-received counter so the operator sees whether the socket was actively streaming before the failure.
  - Files: `crates/core/src/websocket/connection.rs`
  - Tests: `test_live_feed_connect_log_includes_connection_id_and_sid_range`, `test_live_feed_disconnect_log_includes_frame_stats`

- [ ] **P5.5** **Live Order Update:** Include `client_id` (redacted), last-seen order event timestamp, and reason classification (auth failure vs transport vs silent) in every disconnect log. On each order alert received, log the `CorrelationId`, `OrderNo`, and `OrderStatus` — these are the identifiers that matter for order WS debugging.
  - Files: `crates/core/src/websocket/order_update_connection.rs`
  - Tests: `test_order_update_connect_log_includes_redacted_client_id`, `test_order_update_disconnect_log_classifies_reason`, `test_order_update_alert_log_includes_correlation_id_and_order_no`

- [ ] **P5.6** **Cross-cutting notification events:** Every `NotificationEvent::WebSocket*`, `DepthTwenty*`, `DepthTwoHundred*`, `OrderUpdate*` event must carry a structured payload that the Telegram formatter can render as:
  - Live feed: `[WS #N] 5000 instruments (SID 1333-99999) disconnected: <reason>`
  - Depth 20: `[Depth-20 NIFTY] 50 instruments disconnected: <reason> (last frame T-5s)`
  - Depth 200: `[Depth-200 NIFTY-ATM-CE SID=54816] disconnected: <reason>`
  - Order update: `[OrderUpdate CLIENT=***82] disconnected: <classified_reason>`
  - Files: `crates/core/src/notification/events.rs`
  - Tests: `test_all_4_ws_notification_events_include_identifiers`, `test_all_4_ws_telegram_format_is_correlation_friendly`

### P6 — Banned-Pattern and Compile-Time Enforcement (so regressions can't sneak back in)

- [ ] **P6.1** Add a banned-pattern rule: `.await` on `frame_sender` inside `run_read_loop` is forbidden. Enforced by `.claude/hooks/banned-pattern-scanner.sh`.
  - Files: `.claude/hooks/banned-pattern-scanner.sh`
  - Test: `test_banned_pattern_rejects_ws_read_loop_backpressure_await`

- [ ] **P6.2** Add a compile-time `const _: () = assert!(...)` proving the WS read timeout constant is NOT used for a client-side deadline.
  - Files: `crates/core/src/websocket/connection.rs`
  - Test: compile fails if someone re-adds a read deadline.

- [ ] **P6.3** Add a `cargo llvm-cov` gate raising coverage to 100% for the touched files.
  - Files: `quality/crate-coverage-thresholds.toml`

### P7 — Chaos & Loom Test Battery (proof of guarantees)

- [ ] **P7.1** `loom` model: WS reader + consumer + spill, proving no possible interleaving blocks the reader.
  - Files: `crates/core/tests/loom_ws_decoupling.rs` (new)

- [ ] **P7.2** Mock WebSocket server chaos: random 1-5s silence periods (no frames, no pings), random TCP RST, random slow-consume. Assert: WS reader logs zero read timeouts, zero drops, WAL has exactly N frames, recovery replays exactly N frames.
  - Files: `crates/core/tests/chaos_ws_server.rs` (new)

- [ ] **P7.3** Chaos: QuestDB container killed for 60s mid-session. Assert: WS reader frame count continues to rise, WAL absorbs all frames, Telegram gets 1 "QuestDB slow" alert (NOT one per liveness check), on QuestDB recovery WAL drains and all frames land in QuestDB with zero duplicates.
  - Files: `crates/storage/tests/chaos_questdb_down_60s.rs` (new)

- [ ] **P7.4** Chaos: disk-full on WAL partition. Assert: graceful halt with CRITICAL alert, no panic, no corrupted WAL record.
  - Files: `crates/storage/tests/chaos_wal_disk_full.rs` (new)

- [ ] **P7.5** `dhat`: prove zero allocation on the WS → WAL path per frame.
  - Files: `crates/core/tests/dhat_ws_reader_zero_alloc.rs` (new)

- [ ] **P7.6** `criterion`: benchmark WS reader throughput under 50K frames/sec. Must sustain with < 100ns/frame overhead on the WAL append.
  - Files: `crates/core/benches/ws_reader_throughput.rs` (new)
  - Budget: `quality/benchmark-budgets.toml` — `ws_reader_frame_handle_ns = 100`

### P8 — Gap Detection and Alerting (handles physical disconnects we can't prevent)

- [ ] **P8.1** Ensure `tick_gap_tracker` fires Telegram for any security_id gap > 5s AND for any WS reconnect event (even if graceful), with the connection_id, downtime duration, and count of missed ticks estimated from historical rate.
  - Files: `crates/trading/src/risk/tick_gap_tracker.rs`, `crates/core/src/notification/events.rs`
  - Tests: `test_gap_tracker_alerts_on_ws_reconnect`, `test_gap_tracker_reports_estimated_missed_ticks`

- [ ] **P8.2** On WS reconnect, trigger a one-shot historical candle fetch for the gap window so the tick gap is backfilled from Dhan REST (covering the physically-unavoidable reconnect window).
  - Files: `crates/core/src/historical/candle_fetcher.rs`, `crates/core/src/websocket/connection.rs`
  - Tests: `test_reconnect_triggers_gap_backfill`, `test_gap_backfill_is_idempotent`

### P9 — Plan Verification

- [ ] **P9.1** Run `bash .claude/hooks/plan-verify.sh` — all items `[x]`, all tests exist, all files modified, all banned-pattern rules added.
- [ ] **P9.2** Run `FULL_QA=1 make scoped-check` — full workspace test battery green.
- [ ] **P9.3** Run `cargo llvm-cov` — 100% line coverage on every touched file.
- [ ] **P9.4** Run `make bench` — no regression on `tick_binary_parse` or `full_tick_processing`; new `ws_reader_frame_handle_ns` under 100ns budget.

---

## Scenarios covered (mechanical checklist)

| # | Scenario | What's tested | Expected |
|---|---|---|---|
| 1 | QuestDB container paused 60s mid-session | `chaos_questdb_down_60s.rs` | 5 WS connections stay UP, zero reconnects, WAL absorbs all frames, on QuestDB recovery all frames replay, zero duplicates |
| 2 | Tick processor deliberately stalled for 120s | `loom_ws_decoupling.rs` + integration | WS reader continues reading frames, WAL grows, zero WS disconnects |
| 3 | 60s silence from mock Dhan server (no frames, pings still flowing) | `chaos_ws_server.rs` | Zero false "read timeout" disconnects, WS connection stays open |
| 4 | 60s silence INCLUDING pings (truly dead socket) | `chaos_ws_server.rs` | Watchdog fires at 50s, connection closed cleanly, reconnect within 200ms, gap backfill triggered |
| 5 | SIGKILL the process mid-ingestion | `chaos_sigkill_replay.rs` (existing + extended) | WAL survives, restart replays, QuestDB has all frames |
| 6 | Disk full on WAL partition | `chaos_wal_disk_full.rs` | Graceful halt, CRITICAL alert, no panic, no corrupted WAL |
| 7 | Slow SELECT 1 on QuestDB (12s) | `test_liveness_single_slow_query_does_not_alert` | No alert fired, liveness gauge toggles briefly |
| 8 | 3 consecutive failed SELECT 1 | `test_liveness_three_consecutive_failures_alerts` | ONE Telegram alert fired (not three) |
| 9 | 200-depth NIFTY CE reset | `test_depth_200_logs_include_security_id_on_reset` | Log line has `security_id=54816`, Telegram has `NIFTY ATM-CE (SID=54816) disconnected: <reason>` |
| 10 | 100K tick/sec burst on single WS connection | `chaos_ws_server.rs` high-rate mode | Zero drops, zero disconnects, WS reader overhead <100ns/frame |
| 11 | Corrupted WAL record (truncated or bad CRC) | `test_wal_corrupted_crc_is_skipped_with_counter` | Record skipped, counter incremented, replay continues |
| 12 | Dhan sends TCP RST (simulated) | `chaos_ws_server.rs` | WS reader returns Err, reconnect triggered within 200ms, gap backfill runs |
| 13 | Banned pattern: someone re-adds `.await` on frame_sender | `test_banned_pattern_rejects_ws_read_loop_backpressure_await` | commit hook blocks |
| 14 | Cross-day WAL files | `test_wal_rollover_on_ist_midnight` | New WAL segment, old one sealed, both replay on startup |

## What is explicitly NOT in scope (physical limits)

| Out of scope | Reason | Mitigation |
|---|---|---|
| Preventing all TCP RSTs from Dhan's LB | We don't control Dhan's infrastructure | Gap backfill via historical candles (P8.2) |
| Replaying ticks Dhan emitted during disconnect | Dhan's WebSocket protocol has no sequence/resend | Gap backfill + Telegram alert if > threshold |
| Preventing QuestDB process crashes | It's a database | WAL absorbs the outage window; docker watchdog restarts the container (already done) |
| 100% line coverage on hardware-dependent paths (e.g., `std::fs::sync_data` failure) | Can't force real fsync failures in tests | Mocked fsync error test + TEST-EXEMPT with justification |

---

## Files touched (for `plan-verify.sh`)

**New:**
- `crates/core/src/websocket/frame_spill.rs`
- `crates/core/src/websocket/watchdog.rs`
- `crates/storage/src/ws_frame_wal.rs`
- `crates/core/tests/loom_ws_decoupling.rs`
- `crates/core/tests/chaos_ws_server.rs`
- `crates/core/tests/dhat_ws_reader_zero_alloc.rs`
- `crates/core/benches/ws_reader_throughput.rs`
- `crates/storage/tests/chaos_questdb_down_60s.rs`
- `crates/storage/tests/chaos_wal_disk_full.rs`

**Modified:**
- `crates/core/src/websocket/connection.rs`
- `crates/core/src/websocket/depth_connection.rs`
- `crates/core/src/notification/events.rs`
- `crates/common/src/constants.rs`
- `crates/common/src/config.rs`
- `crates/app/src/main.rs`
- `crates/app/src/infra.rs`
- `crates/trading/src/risk/tick_gap_tracker.rs`
- `crates/core/src/historical/candle_fetcher.rs`
- `.claude/hooks/banned-pattern-scanner.sh`
- `quality/crate-coverage-thresholds.toml`
- `quality/benchmark-budgets.toml`

---

## Risks & open questions (I need your answers before I start)

1. **WAL location:** `/var/lib/tickvault/wal/` on host, bind-mounted into the app container? Or a dedicated EBS volume in prod? On Mac dev, `./data/wal/`. Preference?
2. **WAL retention:** how long do we keep WAL segments after QuestDB has durably ingested them? Default: delete segment once every record inside it has been confirmed by QuestDB dedup. Acceptable, or do you want 7-day retention for audit?
3. **Watchdog threshold:** `50s` (40s server timeout + 10s buffer) acceptable? This is the ONLY "deadline" in the entire WS pipeline, and even this does not block the read loop — it's a sidecar check.
4. **Gap-backfill scope:** On every reconnect, should we backfill **all** subscribed instruments via 1-minute candles, or only those that had recent ticks? Full backfill is safer but hits Dhan data API rate limits harder.
5. **Zero-drop promise on the last-resort counter:** `tv_ws_frame_spill_drop_critical` should ALWAYS be zero. If it ever ticks, I treat it as a P0 bug. Confirm you want this as the contract.

---

## Estimated work size

- **9 new files** (~3,500 LoC prod + ~4,500 LoC tests)
- **12 modified files** (~800 LoC delta)
- **~70 new tests** (unit + integration + loom + chaos + dhat + criterion)
- Multi-step implementation — each P-item is its own commit for bisectability.

Do NOT start implementation until this plan is marked **APPROVED** by Parthiban.

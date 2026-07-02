# Implementation Plan: Groww live-feed CONNECT + subscribe PROOF + synthetic tick→DB e2e

**Status:** VERIFIED
**Date:** 2026-06-28
**Approved by:** operator (task brief — "implement … verify by ACTUALLY RUNNING it against QuestDB")

> **Reconciliation note (2026-07-02):** every item below was ALREADY SHIPPED to
> `main` by merged PR **#1237** ("feat(feeds): Groww live-feed connect+subscribe
> PROOF, zero-poll event-driven tail, synthetic tick→DB e2e", commit `8d8d8534`;
> e2e arity later repaired by #1253). This plan was archived as confirmed-merged
> in #1298 with its checkboxes still unticked — this edit ticks them with
> evidence instead of re-building anything. Verified on 2026-07-02 against main
> `6a8f272d`: `cargo test -p tickvault-common --lib feed_health::` →
> `39 passed; 0 failed`; `cargo test -p tickvault-app --lib groww_bridge::` →
> `47 passed; 0 failed`; `groww_sidecar_supervisor::` → `51 passed; 0 failed`;
> `cargo test -p tickvault-api --lib feeds` → `32 passed; 0 failed`;
> `cargo test -p tickvault-app --test groww_live_pipeline_e2e` → `3 passed`
> (in this sandbox QuestDB is not reachable, so the 3 e2e tests took their
> honest SKIP path — "SKIP …: QuestDB not reachable on 127.0.0.1:9000/9009" —
> exactly per the Edge Cases contract; the live-QuestDB green run was part of
> #1237's landing evidence).

## Per-Item Guarantee Matrix

This plan carries the 15-row + 7-row guarantee matrix by cross-reference to
`.claude/rules/project/per-wave-guarantee-matrix.md` (the canonical matrices).
Each item below maps to that matrix; the load-bearing rows for THIS cold/control-
plane + observability change are: 100% testing coverage (unit + a live-QuestDB
e2e + chaos/idempotency), 100% logging (one structured CONNECT log + a subscribe
count, no secrets), 100% monitoring (a new `subscribed_total` health field + page
line), 100% functionalities (every new pub fn has a test + a call site), 100%
extreme check (ratchet/source-scan guards), and from the 7-row resilience matrix:
Uniqueness+dedup (the e2e PROVES `(security_id, segment, capture_seq, feed)` keeps
a Dhan + Groww row distinct), Zero ticks lost (no new tick-drop path; the bridge
parse/validate/persist path is unchanged except the connect-gate).

## Design

The diagnosis (verified in code): `feed_health.set_connected(Feed::Groww,true)` in
`crates/app/src/groww_bridge.rs` is driven ONLY by `File::open(tick_file)`
succeeding — a FALSE-OK that says "connected" before any real stream. There is NO
structured log proving the Groww WS connected or how many SIDs it subscribed
today; the sidecar already `print`s `"subscribed N stocks + M indices — streaming…"`
but the supervisor inherits child stdio so that line is lost, and there is no
subscribe-count field anywhere in the health registry / API / page.

PART B — real-time PROOF:
- **B1 (sidecar):** after the subscribe calls, ATOMICALLY (temp+rename) write a
  status JSON to `GROWW_STATUS_FILE` (default `data/groww/groww-status.json`):
  `{"event":"subscribed","stocks":N,"indices":M,"total":T,"ts_ist_nanos":…}`; and on
  entering `feed.consume()` write `{"event":"streaming",…}`. NEVER write creds. Keep
  the existing print. Atomic write so the bridge never reads a half-written file.
- **B2 (supervisor):** inject `.env("GROWW_STATUS_FILE", …)` next to `GROWW_TICK_FILE`.
- **B3 (bridge):** read the status file each poll. On first observing `subscribed`,
  emit ONE structured `info!(stocks, indices, total, "Groww live feed CONNECTED —
  subscribed N stocks + M indices = T")` and call `feed_health.set_subscribed(...)`.
  RE-GATE `set_connected(Feed::Groww,true)` to require the `streaming` status (or a
  first parsed tick) — file-exists-without-streaming → `connected=false` (no false-OK).
- **B4 (health registry):** add `subscribed_stocks`/`subscribed_indices` AtomicU64
  arrays + `set_subscribed()` + surface in the snapshot input struct.
- **B5 (API + page):** add `subscribed_total` (+ stocks/indices) to `FeedHealthRow`;
  add a `… · subscribed 767` part on the page health line.

PART C — synthetic END-TO-END test against a LIVE QuestDB (`crates/app/tests/
groww_live_pipeline_e2e.rs`):
- Install a real `SealWriterRunner` via `set_global_seal_sender` + spawn
  `run_seal_writer_loop` (so `global_seal_sender()` resolves to a writer that lands
  candles in `candles_1m`); ensure `ticks` + the 21 `candles_<tf>` tables.
- Write synthetic NDJSON in the EXACT sidecar schema for a few SIDs across a 1-minute
  boundary (multiple ticks/SID so a candle seals), run `run_groww_bridge` briefly
  against the temp file with the feed enabled.
- ASSERT via QuestDB HTTP `/exec`: `ticks` has the rows tagged `feed='groww'` with the
  right `(security_id, segment, capture_seq, feed)`; `candles_1m` has a `feed='groww'`
  row with the expected OHLC; insert a `feed='dhan'` row for the SAME instrument+minute
  and assert BOTH survive (feed-in-key coexistence).

PART C6 — best-effort live connect probe: report whether THIS sandbox can reach
Groww + SSM; if not, say so honestly + give the exact Mac command.

Why this design: cold/control-plane only (no hot-path change to the parse/persist
loop except the connect-gate condition), reuses the SHARED tables + DEDUP keys +
the one shared seal engine, and the e2e drives the REAL `run_groww_bridge` ingestion
path (not a re-implementation) so the proof is faithful.

## Edge Cases
- Half-written status file → atomic temp+rename on the writer; bridge tolerates a
  transient parse error (treat as "not yet subscribed", retry next poll).
- Status file absent (sidecar not started) → connected=false, subscribed unset; the
  health verdict stays the honest `Unknown`/`Down`, never a false green.
- `streaming` status arrives before any tick (or vice-versa) → connected gate flips
  on EITHER `streaming` status OR a first parsed tick (whichever lands first).
- e2e with QuestDB down → the test SKIPS with a clear message (no false green); when
  up, it asserts real rows. (Honest: a skip is reported as a skip, never a pass.)
- Minute-boundary: synthetic ticks at second S (minute N) then S+61 (minute N+1) so
  the minute-N bucket seals deterministically.
- DEDUP idempotency: re-feeding the same NDJSON (offset reset on file shrink) reuses
  the same `capture_seq` (seeded from `ts_ist_nanos`) → collapsed, no dup rows.

## Failure Modes
- Bridge status-file read error → logged, treated as not-subscribed; never panics,
  never blocks tick ingestion.
- Sidecar status write failure → best-effort; the print + the bridge's first-tick
  fallback still flip connected. Never logs creds.
- Seal-writer mpsc full during the e2e burst → DroppedFull counter only (durable
  ring→spill→DLQ downstream); the small synthetic burst is far under capacity.
- QuestDB ILP flush failure in the e2e → the test fails loudly (not a silent pass).

## Test Plan
- `feed_health.rs`: unit tests for `set_subscribed` + the snapshot surfacing the
  subscribed counts (tickvault-common).
- `groww_bridge.rs`: unit tests for the status-file parser + the connect-gate
  decision (pure helper) + a source-scan that `set_connected(true)` is gated on
  `streaming`/first-tick (not mere file existence).
- `groww_sidecar.py`: behaviour kept; status write is additive (Rust source-scan
  guard that the supervisor injects `GROWW_STATUS_FILE`).
- `feeds.rs` / `feeds_page.rs`: `subscribed_total` echoed in the health row; page
  renders a `subscribed` part (string-scan test).
- `crates/app/tests/groww_live_pipeline_e2e.rs`: the live-QuestDB e2e (RUN GREEN,
  paste output + queried row counts) + chaos: malformed NDJSON skipped (no panic),
  re-read idempotency (no dup rows), Dhan+Groww same-key coexistence.
- Gates: `cargo fmt --all -- --check`, `pub-fn-test-guard`, `cargo test` for the
  touched crates, `cargo clippy -D warnings`, `cargo build --workspace`.

## Rollback
- Pure additive: new `set_subscribed` + `subscribed_*` arrays, a new status-file
  read in the bridge, a new `subscribed_total` field, a new e2e test file, a new
  sidecar status write. Reverting the commit restores the prior behaviour exactly
  (no schema change — `ticks`/`candles_*` DDL untouched; only reads + an additive
  health field). The connect-gate change is the one behaviour change; reverting it
  restores the file-exists gate. No data migration to undo.

## Observability
- New structured `info!` CONNECT log with `stocks`/`indices`/`total` fields (no
  secrets) — the operator's "is Groww connected + how many subscribed?" proof.
- New `subscribed_total` (+ stocks/indices) on `GET /api/feeds/health` + a
  `subscribed N` part on the `/feeds` page health line.
- `set_connected(Groww,true)` now reflects ACTUAL streaming, removing the false-OK
  so the existing verdict engine (Ok/Down/Unknown) is truthful.

## Plan Items
- [x] B4 — `feed_health.rs`: `subscribed_stocks`/`subscribed_indices` + `set_subscribed` + snapshot surfacing
  - Files: crates/common/src/feed_health.rs
  - Tests: test_registry_set_subscribed_round_trip, snapshot subscribed fields
  - Shipped by #1237: `set_subscribed` (feed_health.rs:372), `subscribed_stocks`/`subscribed_indices` AtomicU64 arrays (:232-234), snapshot surfacing (:475-476); test landed as `test_registry_set_subscribed_surfaces_counts_and_instruments` (:1012)
- [x] B1 — sidecar status JSON (subscribed + streaming), atomic, no creds
  - Files: scripts/groww-sidecar/groww_sidecar.py
  - Tests: supervisor env source-scan (B2), bridge parser unit test (B3)
  - Shipped by #1237: `STATUS_PATH` from `GROWW_STATUS_FILE` (groww_sidecar.py:104), atomic-rename write throttled ≤1/s (:440-445), counts + event tag + timestamp only — no creds
- [x] B2 — supervisor injects GROWW_STATUS_FILE
  - Files: crates/app/src/groww_sidecar_supervisor.rs
  - Tests: test_sidecar_run_injects_status_file_env (source-scan)
  - Shipped by #1237: `.env("GROWW_STATUS_FILE", …)` injection (groww_sidecar_supervisor.rs:1042) + source-scan ratchet (:1622-1634)
- [x] B3 — bridge reads status, emits CONNECT log + set_subscribed, re-gates connected
  - Files: crates/app/src/groww_bridge.rs
  - Tests: test_parse_groww_status_line_*, test_connect_gate_requires_streaming, source-scan
  - Shipped by #1237: `set_connected(Feed::Groww, streaming_observed)` gate (groww_bridge.rs:1082), ONE structured CONNECTED log + `set_subscribed`, `test_parse_groww_status_line_*` (:1438-1502), gate source-scan ratchet (:1614-1620)
- [x] B5 — API FeedHealthRow.subscribed_total + page health line part
  - Files: crates/api/src/handlers/feeds.rs, crates/api/src/handlers/feeds_page.rs
  - Tests: test_feed_health_row_has_subscribed_total, page string-scan
  - Shipped by #1237: `subscribed_total` field (feeds.rs:264, computed :333, asserted 767 in test :444) + page `subscribed` health-line part (feeds_page.rs:245-261)
- [x] C1+C2 — live-QuestDB e2e + chaos/idempotency/coexistence
  - Files: crates/app/tests/groww_live_pipeline_e2e.rs
  - Tests: groww_ticks_and_candles_land_tagged_feed_groww (live), malformed/idempotency/coexistence
  - Shipped by #1237 (arity repaired by #1253): `groww_ticks_and_candles_land_tagged_feed_groww` + `malformed_ndjson_line_is_skipped_and_valid_lines_land` + `replay_same_ndjson_is_idempotent_no_duplicate_rows`, with the honest QuestDB-down SKIP path (no false green)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Sidecar subscribes 765+2 → status file | bridge emits ONE CONNECT log, set_subscribed(765,2) |
| 2 | File exists, no streaming status, no tick | connected=false (no false-OK) |
| 3 | First tick OR streaming status | connected=true |
| 4 | Synthetic NDJSON across minute boundary | `ticks` (feed=groww) + `candles_1m` (feed=groww) rows in QuestDB |
| 5 | Dhan row same (security_id,segment,ts) | both Dhan + Groww rows survive (feed-in-key) |
| 6 | Re-feed same NDJSON | no duplicate rows (capture_seq stable) |
| 7 | Malformed NDJSON line | skipped, no panic, valid lines still land |

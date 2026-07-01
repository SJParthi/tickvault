# Implementation Plan: Groww honest streaming/connected flag + decoded-but-dropped visibility

**Status:** APPROVED
**Date:** 2026-06-29
**Approved by:** operator standing directive 2026-06-29 ("fix the fake-connected
signal — only show streaming on a REAL decoded tick, make the silent drop visible,
and make it merge-ready")

## Per-Item Guarantee Matrix

This plan carries the 15-row + 7-row guarantee matrix by cross-reference to
`.claude/rules/project/per-wave-guarantee-matrix.md` (the canonical matrices).
Each item below maps to that matrix; the load-bearing rows for THIS
cold/control-plane + observability change are: 100% testing coverage (Rust
unit + the existing live-QuestDB e2e), 100% logging (no new secret-bearing log;
the existing structured CONNECT log is unchanged), 100% monitoring (two new
`decoded_emitted`/`decoded_dropped` health fields + a `decoded N · dropped M`
page line), 100% functionalities (every new pub fn has a test + a call site),
100% extreme check (round-trip + source-scan/parse guards), and from the 7-row
resilience matrix: Uniqueness+dedup (DEDUP keys + `ticks` schema UNCHANGED — this
is a status/health-surface change only), Zero ticks lost (no new tick-drop path;
the bridge parse/validate/persist path is byte-unchanged — the only change makes
the sidecar's ALREADY-EXISTING `continue` drops COUNTED + VISIBLE instead of
silent, and moves the optimistic `streaming` write to fire on the FIRST real
emit). The changed crates are **app** (`groww_bridge.rs`), **common**
(`feed_health.rs`), **api** (`feeds.rs` + `feeds_page.rs`), and the **Python
sidecar** (`scripts/groww-sidecar/groww_sidecar.py`).

## Design

The diagnosis (verified in code, file:line):

1. `scripts/groww-sidecar/groww_sidecar.py:486` writes
   `write_status("streaming", …)` IMMEDIATELY BEFORE `feed.consume()` at :487 —
   an OPTIMISTIC false-OK: it claims "streaming" before a single tick has been
   decoded/emitted. The Rust bridge
   (`crates/app/src/groww_bridge.rs:774-791`) turns that into
   `feed_health.set_connected(Feed::Groww, true)` via `streaming_observed`, so the
   `/feeds` page shows green BEFORE any data flows.
2. `emit_ltp_records` (~:337-350) and `emit_index_records` (~:367-379) silently
   `continue` (DROP) a decoded record when the `sid_map` key misses or the
   `ltp`/`value` field is absent. A whole feed of unmapped tokens would decode but
   emit ZERO ticks — and the operator would see "streaming" with "0 ticks" and no
   clue WHY.

THE FIX (sidecar — the producer):
- **REMOVE** the optimistic pre-consume `write_status("streaming", …)`.
- Add module-level counters `EMITTED_TOTAL` / `DROPPED_TOTAL`.
- In BOTH `emit_ltp_records` + `emit_index_records`: count every DROP
  (`sid_map` miss OR missing `ltp`/`value`) into `DROPPED_TOTAL`; count every
  successful `_write_record` emit into `EMITTED_TOTAL`.
- On the FIRST successful emit (`EMITTED_TOTAL` goes 0→1) write
  `write_status("streaming", stocks, indices)` — now the ONLY place "streaming" is
  written, backed by a REAL decoded+emitted tick. Re-write the status (refreshed
  `emitted`/`dropped`) at a THROTTLED cadence (on first emit, then at most once/sec)
  so the bridge can surface counts without status-file thrash.
- Extend `write_status(...)` JSON with integer `emitted` + `dropped` fields. Keep
  the atomic temp+rename write; NEVER write a credential. Keep the post-subscribe
  `write_status("subscribed", …)` (honest "attempted").

THE FIX (Rust — the consumer):
- **`feed_health.rs`:** add `decoded_emitted` + `decoded_dropped` `[AtomicU64;
  Feed::COUNT]` arrays + a `set_decode_counts(feed, emitted, dropped)` setter
  (matching the existing AtomicU64-array pattern) + surface both in
  `FeedHealthInput` / `FeedHealthReport` snapshot. Display-only — they do NOT
  change the verdict (a subscribed-but-silent feed is still Down).
- **`groww_bridge.rs`:** parse the new `emitted`/`dropped` fields from the status
  JSON (`GrowwStatusLine`); call `feed_health.set_decode_counts(...)` alongside
  the existing `set_subscribed(...)`. `connected` stays gated on
  `streaming_observed` — which is now HONEST because the sidecar only writes
  `streaming` on a real emit. The zero-poll notify watcher + the disable-branch
  latch reset are UNTOUCHED.
- **`feeds.rs`:** add `decoded_emitted` + `decoded_dropped` to `FeedHealthRow`
  from the snapshot.
- **`feeds_page.rs`:** when subscribed but not streaming / no tick, the page now
  reads the honest verdict (Down/Unknown) already; when `decoded_dropped > 0`,
  render `decoded N · dropped M` so a key-map mismatch is visible at a glance.

Why this design: cold/control-plane + observability only (no hot-path change to
the tick parse/persist loop). The streaming claim becomes provable; the
already-existing silent drop becomes a counted, surfaced number. No schema
change, no DEDUP-key change, no new tick-drop path.

## Edge Cases
- Status file has no `emitted`/`dropped` (old sidecar) → serde `#[serde(default)]`
  → both read 0; the page simply shows no `decoded`/`dropped` part. Forward-safe.
- `emitted`/`dropped` arrive before any `subscribed` event → counts still recorded
  via `set_decode_counts`; `set_subscribed` runs on the same status read.
- Half-written status file → atomic temp+rename on the writer; bridge tolerates a
  transient parse error (treat as "not yet", retry next poll). UNCHANGED.
- First-emit streaming write races a tick: `connected` flips on EITHER the
  `streaming` status OR the first parsed tick (bridge already does both). UNCHANGED.
- Sidecar drops every record (all tokens unmapped) → `emitted=0`, `dropped>0`,
  NO `streaming` status ever written → `connected` stays false → honest Down, with
  `dropped M` shown so the operator sees the cause.
- Status throttle: first emit always writes; subsequent writes at most once/sec so
  a fast tick stream cannot thrash the atomic-rename path.

## Failure Modes
- Sidecar status write failure → best-effort (logged, type only, never creds);
  the stream itself is unaffected; the bridge's first-parsed-tick fallback still
  flips `connected`. UNCHANGED.
- Bridge status-file read/parse error → logged, treated as "not yet"; never
  panics, never blocks tick ingestion. UNCHANGED.
- A decoded record with a missing field → counted as a drop, skipped; the valid
  records in the same callback still emit (no whole-batch failure).
- Old sidecar + new bridge (mixed deploy) → bridge reads `emitted=0,dropped=0`
  (serde default), page shows no decode line — degrades cleanly, no error.

## Test Plan
- `feed_health.rs` (tickvault-common): unit test that `set_decode_counts`
  round-trips through the snapshot (emitted + dropped), marks instrumented, and is
  per-feed isolated; does NOT by itself change the verdict.
- `groww_bridge.rs` (tickvault-app): extend the status-parse tests — `subscribed`
  does NOT flip connected, `streaming` DOES, and the new `emitted`/`dropped`
  fields parse (incl. the `#[serde(default)]` absent-field case). A source-scan
  that the bridge calls `set_decode_counts` from the status read.
- `groww_sidecar.py`: NO pytest harness exists under `scripts/groww-sidecar/`
  (verified: only `groww_sidecar.py`, `groww_smoke.py`, `requirements.txt`,
  `README.md`). Per the brief, SKIP inventing a Python unit test; the behaviour is
  pinned Rust-side by the parser tests + the live e2e. A Rust source-scan guard
  asserts the sidecar no longer writes `streaming` before `consume()` and counts
  drops.
- `crates/app/tests/groww_live_pipeline_e2e.rs`: kept GREEN (unchanged — the
  ingestion path is byte-unchanged).
- `feeds.rs` / `feeds_page.rs`: `decoded_emitted`/`decoded_dropped` echoed in the
  health row; page renders a `decoded`/`dropped` part (string-scan test).
- Gates: `cargo fmt --all`, `cargo clippy -p tickvault-app -p tickvault-common
  -p tickvault-api -- -D warnings`, `cargo test -p tickvault-common -p
  tickvault-app -p tickvault-api`.

## Rollback
Pure additive on the Rust side: new `set_decode_counts` + `decoded_*` arrays, a
new pair of status fields parsed, two new `FeedHealthRow` fields, a new page line.
The one behaviour change is the sidecar moving `write_status("streaming")` from
pre-`consume` to first-emit + counting drops. Reverting the commit restores the
prior (optimistic) behaviour exactly — no schema change (`ticks`/`candles_*` DDL
untouched), no DEDUP-key change, no data migration to undo.

## Observability
- The `streaming`/`connected` flag is now PROVABLE: green only after a real
  decoded+emitted tick (the false-OK is removed at the source).
- New `decoded_emitted` + `decoded_dropped` on `GET /api/feeds/health` + a
  `decoded N · dropped M` part on the `/feeds` page health line — a previously
  SILENT sid-map mismatch (decode-but-drop) is now a visible number, so "streaming
  but 0 ticks" has an immediate, plain-English cause.

## Plan Items
- [ ] feed_health.rs — `decoded_emitted`/`decoded_dropped` + `set_decode_counts` + snapshot surfacing
  - Files: crates/common/src/feed_health.rs
  - Tests: test_registry_set_decode_counts_round_trip
- [ ] sidecar — count drops/emits; move `streaming` to first-emit; add emitted/dropped to status JSON
  - Files: scripts/groww-sidecar/groww_sidecar.py
  - Tests: Rust source-scan guard (no pytest harness present — skipped, noted)
- [ ] bridge — parse emitted/dropped, call set_decode_counts; connected stays streaming-gated
  - Files: crates/app/src/groww_bridge.rs
  - Tests: test_parse_groww_status_line_decode_counts, test_parse_groww_status_line_decode_counts_default, source-scan
- [ ] API + page — FeedHealthRow.decoded_emitted/decoded_dropped + page `decoded · dropped` line
  - Files: crates/api/src/handlers/feeds.rs, crates/api/src/handlers/feeds_page.rs
  - Tests: test_get_feeds_health_reflects_decode_counts, page string-scan

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Sidecar subscribes, no tick yet | NO `streaming` status, connected=false (no false-OK) |
| 2 | First record emits | sidecar writes `streaming`, bridge flips connected=true |
| 3 | All tokens unmapped (decode-but-drop) | emitted=0, dropped>0, connected=false, page shows `dropped M` |
| 4 | Mixed: some emit, some drop | streaming after first emit; page shows `decoded N · dropped M` |
| 5 | Old sidecar status (no emitted/dropped) | serde default 0; page shows no decode line; no error |
| 6 | Status read parse error | treated as not-yet; never panics, no tick-ingestion block |

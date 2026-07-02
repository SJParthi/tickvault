# Implementation Plan: Groww QuestDB tick-writer reconnect + honest persisted-row counter

**Status:** VERIFIED (reconciled 2026-07-02 — audit verified all items shipped on origin/main)
**Date:** 2026-06-29
**Approved by:** Parthiban (operator) — grounded directive, this session

> Cross-ref: `.claude/rules/project/per-wave-guarantee-matrix.md` (the 15-row +
> 7-row matrices below are carried per that mandatory contract).

## Design

**The bug (code-proven this session):** `GrowwLiveTickWriter`
(`crates/storage/src/groww_persistence.rs`) is fire-once with NO reconnect. If
`Sender::from_conf` fails at `new()` it sets `sender=None` and stays None for the
whole process. Every `append_row` still succeeds into the in-RAM ILP `Buffer`
(and the dashboard counter `feed_health.record_tick(Feed::Groww)` increments per
append), but every `flush()` returns `Err` because the sender is None → the
buffer never drains → **0 rows in `ticks` for feed='groww'**, while the dashboard
shows hundreds of thousands of "ticks". The bridge is spawned (`main.rs`) BEFORE
the QuestDB-ready gate, so the sender frequently constructs before QuestDB is up.

**The fix (minimal, mirrors the Dhan `TickPersistenceWriter` resilience pattern):**

1. **Lazy reconnect in `GrowwLiveTickWriter`** — retain the ILP conf string in the
   struct (exactly like `TickPersistenceWriter::ilp_conf_string`). In `flush()`:
   if `sender.is_none()`, attempt `Sender::from_conf(&self.ilp_conf)` to
   (re)establish before flushing; on reconnect failure return `Err` with the
   buffer RETAINED (no row dropped). On a flush `Err` from an EXISTING sender, set
   `self.sender = None` so the next flush reconnects (drop-and-reconnect),
   preserving the buffer. `flush()` returns `Ok(usize)` = the number of rows
   actually flushed, so the bridge can count PERSISTED rows.

2. **Honest counter** — move `feed_health.record_tick(Feed::Groww, …)` OFF the
   per-`append_row` site and ONTO the post-successful-`flush` site, incrementing
   by the count of rows actually written to QuestDB. A new
   `FeedHealth::record_ticks(feed, n, ts)` does the `fetch_add(n)` in O(1) (one
   atomic add, not N). The dashboard "N ticks" (`ticks_total`, rendered by
   `feeds_page.rs`) becomes "rows actually written to QuestDB".

3. **Dhan path untouched** — only the Groww (feed='groww') accounting changes. The
   Dhan `TickPersistenceWriter` + its `record_tick` semantics are not modified.

**O(1) honesty:** per-tick `append_row` stays O(1) zero-alloc. `flush()` is the
cold path (once per bridge wake, not per tick); reconnect is `from_conf` only when
disconnected. `record_ticks` is a single `fetch_add` — O(1), not O(N).

## Edge Cases

- QuestDB down at `new()` → sender None, ticks buffer, first successful `flush()`
  after reconnect drains the whole buffer + counts all of them at once.
- QuestDB drops mid-session → existing-sender flush Err → sender=None, buffer
  retained, next flush reconnects.
- Reconnect itself fails → flush returns Err, buffer retained, counter NOT bumped
  (honest — nothing was persisted).
- Empty buffer / `pending()==0` → `flush()` returns `Ok(0)`; no counter bump.
- `for_test()` writer (disconnected) → flush attempts reconnect to the test conf
  (unreachable) → Err; buffer retained; pending preserved.

## Failure Modes

- Buffer unbounded growth if QuestDB is down for a long time: the durable floor for
  Groww ticks is the producer's capture-at-receipt file (lock §32), so the in-RAM
  ILP buffer is a best-effort second tier — this PR keeps it RETAINED on failure
  (no silent loss) and logs `error!` with a code on flush failure (the operator's
  smoking-gun line is preserved). A hard cap is explicitly OUT of scope for this
  minimal correctness fix (noted as a follow-up).
- Flush failure still logs `error!` (audit Rule 5 / charter Rule 6) so it routes to
  Telegram.

## Test Plan

- Unit (storage): `pending` preserved on disconnected flush; `flush()` returns
  `Ok(0)` for an empty connected-less buffer is not assertable without a live
  sender, so assert the disconnected path Errs and retains buffer+pending; assert
  the writer stores the conf for reconnect; existing append/serialisation tests
  stay green.
- Unit (common): `record_ticks(feed, n, ts)` bumps `ticks_total` by exactly `n`
  and stamps last-tick ts; `record_ticks(feed, 0, ts)` is a no-op on the count.
- `cargo check -p tickvault-storage` + `-p tickvault-app` + `-p tickvault-common`
  clean.
- Live QuestDB e2e (`groww_live_pipeline_e2e`) is NOT runnable in this sandbox (no
  QuestDB) — stated honestly; the reconnect logic is reasoned + unit-covered.

## Rollback

Single, self-contained diff on `groww_persistence.rs` + `groww_bridge.rs` +
`feed_health.rs`. Revert the commit to restore the prior fire-once behaviour. No
schema change, no migration, no config flag. Groww is default-OFF so prod is
unaffected until enabled.

## Observability

- `error!` on flush failure retained (smoking-gun line, routes to Telegram).
- The dashboard `ticks_total` now reflects PERSISTED rows (the honest fix is itself
  the observability improvement).
- No new ErrorCode needed (the existing `error!` lines stay).

---

## 15-row "100% everything" guarantee matrix

| Demand | Mechanical proof artefact | Real-time check |
|---|---|---|
| 100% code coverage | new unit tests for reconnect-conf + record_ticks | `cargo test -p tickvault-storage -p tickvault-common` |
| 100% audit coverage | shared `ticks` table (feed='groww') DEDUP keys unchanged | N/A — no new audit table |
| 100% testing coverage | unit (storage+common); e2e exists (needs live QuestDB) | `cargo test` |
| 100% code checks | banned-pattern + plan-gate + per-item-guarantee hooks | pre-push gates |
| 100% code performance | append stays O(1) zero-alloc; flush cold path; record_ticks = 1 fetch_add | reasoned + DHAT path unchanged |
| 100% monitoring | `feed_health` ticks_total now = persisted rows | /api/feeds/health |
| 100% logging | `error!` on flush failure retained | errors.jsonl |
| 100% alerting | flush-failure error! routes to Telegram | existing routing |
| 100% security | no secrets logged; conf string has no token | security-reviewer pass |
| 100% security hardening | no new attack surface (same ILP conf) | security-reviewer pass |
| 100% bugs fixing | adversarial 3-agent on diff | this PR |
| 100% scenarios covering | down-at-boot / drop-mid-session / reconnect-fail covered | unit + reasoning |
| 100% functionalities covering | record_ticks has test + call site; flush returns count + caller uses it | pub-fn guards |
| 100% code review | adversarial 3-agent before+after | this PR |
| 100% extreme check | unit tests fail build on regression | every commit |

## 7-row "Resilience demand" matrix

| Demand | Honest envelope | Per-item proof |
|---|---|---|
| Zero ticks lost | Buffer RETAINED on flush failure; durable floor = producer capture file (lock §32) | no new drop path; buffer never discarded on transient failure |
| WS never disconnects | N/A — this is the QuestDB ILP writer, not a WS | N/A |
| Never slow/locked/hanged | flush is cold path; reconnect is one `from_conf`; no blocking sleep | no hot-path allocation added |
| QuestDB never fails | ABSORB via lazy reconnect (mirrors Dhan writer) | reconnect on None + drop-and-reconnect on Err |
| O(1) latency | append O(1); record_ticks = single fetch_add | no per-row loop |
| Uniqueness + dedup | shared `ticks` key `(ts, security_id, segment, capture_seq, feed)` unchanged | DEDUP key untouched |
| Real-time proof | dashboard ticks_total now = persisted rows (honest) | /api/feeds/health |

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: the Groww
ILP writer reconnects to QuestDB on the next flush after a disconnect (mirroring
the Dhan `TickPersistenceWriter` lazy-reconnect pattern), the buffer is RETAINED
on any flush/reconnect failure (no silent row loss inside the buffer's RAM
envelope; the producer capture-at-receipt file is the durable floor per lock §32),
and the dashboard "ticks" counter now reflects rows ACTUALLY persisted to QuestDB,
not in-memory buffer appends. Beyond the RAM buffer envelope, a sustained QuestDB
outage is bounded only by host memory — an explicit hard cap / spill is a noted
follow-up, not part of this minimal correctness fix.

## Plan Items

- [x] Add `ilp_conf` field + lazy reconnect to `GrowwLiveTickWriter::flush` (returns `Ok(usize)` rows flushed; buffer retained on failure; drop-and-reconnect on existing-sender Err) — DONE on main: `ilp_conf` + `flush() -> Result<GrowwFlushOutcome>` at `crates/storage/src/groww_persistence.rs:326`
  - Files: crates/storage/src/groww_persistence.rs
  - Tests: test_writer_stores_conf_for_reconnect, test_disconnected_flush_errs_and_retains_buffer
- [x] Add `FeedHealth::record_ticks(feed, n, ts)` (single fetch_add) — DONE on main: `record_ticks` in `crates/common/src/feed_health.rs`
  - Files: crates/common/src/feed_health.rs
  - Tests: test_record_ticks_bumps_total_by_n, test_record_ticks_zero_is_noop
- [x] Move the Groww counter from per-append to post-successful-flush; count persisted rows — DONE on main: post-flush `record_ticks` counting wired in `crates/app/src/groww_bridge.rs`
  - Files: crates/app/src/groww_bridge.rs
  - Tests: covered by storage flush return + common record_ticks unit tests

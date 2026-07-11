# Implementation Plan: PR-R2 — Groww Python-vs-Rust tick parity + latency comparer

**Status:** APPROVED
**Date:** 2026-07-05
**Approved by:** Parthiban (operator) — verbatim goal stated today (2026-07-05):
"check 80 to 100 connections scaling of groww using python live feed and then
latency between python live feed vs internal rust websocket".
**Authority:** `.claude/rules/project/groww-second-feed-scope-2026-06-19.md` §35
("PR-R2 = parity comparer") — the native-Rust SHADOW client (PR-R1) writes
`data/groww/rust-live-ticks.ndjson` "for the future exact per-tick parity
comparer (PR-R2)". This plan IS that comparer.

## Design

An **offline, self-contained Python analysis script**
`scripts/groww-parity/compare.py` (stdlib-only, no new deps, no brutex code).
Python chosen over a Rust bin because: (a) this is a cold offline analysis
tool, never on the tick hot path — the O(1)/DHAT/Criterion battery does not
apply; (b) `scripts/` already hosts sanctioned Python (groww-sidecar, §32)
with a `unittest` convention; (c) zero blast radius on the prod Rust binary
(no `crates/*/src` change).

**Verified line schema (field-for-field, both writers read this session):**
- Python writer `scripts/groww-sidecar/groww_sidecar.py::_write_record` emits
  `{"security_id": int, "segment": str, "ts_ist_nanos": int,
  "exchange_ts_millis": int, "ltp": float, "cum_volume": 0}` +
  OPTIONAL `"capture_ns": int` (UTC epoch nanos, `time.time_ns()` at the
  NATS-callback hook).
- Rust writer `crates/core/src/feed/groww/native/shadow_writer.rs`
  (`format_ndjson_line`) emits the identical keys with `capture_ns` ALWAYS
  present (its own round-trip test pins schema parity against a Python
  sample line).
- There is NO separate `exchange` field on either side — exchange identity is
  folded into the canonical `segment` string (`NSE_EQ`, `IDX_I`, …). The
  join key is therefore `(segment, security_id, exchange_ts_millis)` — the
  honest realization of the requested
  `(exchange, segment, token, exchange_timestamp_ms)` key.

**Algorithm (bounded memory — stream, never slurp):**
1. Stream the Rust file line-by-line; build
   `dict[(segment, security_id, exchange_ts_millis)] -> capture_ns`,
   deduping within-side (first occurrence wins; duplicates counted).
   Memory is O(unique Rust keys) — ~tens of MB at 800K+ lines, flagged
   honestly as O(N-keys), not claimed O(1).
2. Stream the Python file line-by-line; dedupe within-side via a seen-set;
   each unique key found in the Rust dict = MATCHED (record
   `python_capture_ns − rust_capture_ns` when both stamps exist; count
   `capture_ns`-absent lines separately); not found = MISSING-IN-RUST.
3. Rust keys never matched = MISSING-IN-PYTHON.
4. Latency stats over matched deltas: p50/p90/p99/min/max/mean (ms with µs
   precision), overall + per class (index = `IDX_I` segment vs stock =
   everything else).
5. Outputs: human table on stdout (header states the clock basis: BOTH
   `capture_ns` stamps come from the SAME host wall clock —
   `time.time_ns()` in Python, `SystemTime` in Rust — so deltas are
   host-clock-consistent, not cross-host); per-key TSV to
   `data/groww/parity-<IST-date>.tsv`; final Telegram-ready 5-line summary.
6. Malformed lines: counted + sampled (first 3), never crash.

## Edge Cases

- Python line WITHOUT `capture_ns` (old-format / walker emission): counted
  as `matched_no_py_capture`, excluded from latency stats, still counted as
  matched for parity.
- Duplicate key on either side (Groww reconcile-walker re-emission):
  deduped first-wins within side, counted per side.
- `exchange_ts_millis == 0` (absent exchange stamp): counted as
  `unjoinable`, excluded from the join (a zero-ts tick cannot be identified
  as "the same tick").
- Empty file / missing file on one side: report proceeds with the other
  side's totals; parity section states the absence explicitly (no false-OK
  "all match" on an empty compare set — audit Rule 11).
- Malformed JSON / non-dict line / wrong types: counted per side, sampled,
  skipped.
- Negative latency delta (Rust captured AFTER Python): legitimate — reported
  as negative; min/max preserve sign.
- ltp disagreement on a matched key: counted (`ltp_mismatch`) and sampled —
  parity is key-based; a price disagreement on the same tick identity is a
  separate honest signal.

## Failure Modes

- File unreadable / permission error → per-side fatal message, exit 2,
  never a traceback spray.
- TSV output dir missing → created (`os.makedirs(exist_ok=True)`); write
  failure → error to stderr, stdout report still printed, exit 2.
- Pathological duplicate storm → dedup counters expose it; memory stays
  O(unique keys).
- Clock caveat: same-host clock means NTP steps mid-session distort deltas
  equally for both sides at the same instant; stated in the report header.
- The script NEVER touches QuestDB, the live feed, tokens, or any prod
  path — a comparer failure loses only the offline report.

## Test Plan

`scripts/groww-parity/test_compare.py` (unittest, mirrors the
`test_dedup.py` convention) with synthetic NDJSON fixtures:
- [x] perfect match (N ticks both sides) → matched=N, zero missing
- [x] rust missing N → missing_in_rust == N
- [x] python missing N → missing_in_python == N
- [x] known latency deltas → exact p50/p90/p99/min/max asserted
- [x] malformed lines → counted, run completes
- [x] duplicate keys within a side → deduped, counted
- [x] python line without capture_ns → matched but excluded from latency
- [x] per-class (IDX_I vs stock) split correctness
- [x] unjoinable (ts==0) exclusion
- [x] TSV file written with expected rows
- Run: `cd scripts/groww-parity && python3 -m unittest test_compare -v`
  plus `python3 -m py_compile compare.py`.

## Rollback

The script is additive-only (new directory `scripts/groww-parity/`, this
plan file). Rollback = `git revert` of the single commit; no schema, no
config, no prod code, no data migration to undo. The Rust and Python
writers are untouched.

## Observability

The script IS an observability artifact: stdout human table + TSV forensic
file (`data/groww/parity-<IST-date>.tsv`) + Telegram-ready 5-line summary.
It emits no runtime metrics (offline tool); the live-path 7-layer telemetry
contract is unaffected. Malformed/duplicate/unjoinable counters make every
skipped line visible — nothing is silently dropped from the accounting.

## Plan Items

- [x] Verify both writers' NDJSON schema field-for-field
  - Files: scripts/groww-sidecar/groww_sidecar.py (read),
    crates/core/src/feed/groww/native/shadow_writer.rs (read)
- [x] Comparer script (stream join, dedupe, latency stats, 3 output formats)
  - Files: scripts/groww-parity/compare.py
- [x] Synthetic-fixture unit tests (10 scenarios)
  - Files: scripts/groww-parity/test_compare.py
  - Tests: test_perfect_match, test_rust_missing, test_python_missing,
    test_latency_percentiles, test_malformed_lines, test_duplicate_keys,
    test_missing_capture_ns, test_class_split, test_unjoinable_ts_zero,
    test_tsv_output
- [x] README for the operator run command
  - Files: scripts/groww-parity/README.md

## Per-Item Guarantee Matrix

Cross-reference: `.claude/rules/project/per-wave-guarantee-matrix.md` (the
canonical 15-row + 7-row matrices apply). Sentinels + this item's specifics:
- 100% code coverage: the comparer's join/stat/parse logic is exercised by
  10 unittest scenarios over synthetic fixtures (offline tool — outside the
  Rust llvm-cov ratchet by construction; Rust floors unchanged).
- Zero ticks lost: this PR adds NO tick path — it READS the two existing
  capture files; the ring→spill→DLQ chain and both writers are untouched.
- Honest envelope: "100% inside the tested envelope" — the comparer proves
  parity/latency over the ticks PRESENT in the two capture files; ticks
  Groww never delivered to either client are invisible (the §5 CAPTURE vs
  UPSTREAM split of groww-second-feed-scope-2026-06-19.md holds).

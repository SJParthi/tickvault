# Implementation Plan: Per-Feed Latency + Metrics Comparison (Operator Portal)

**Status:** APPROVED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — verbatim demand 2026-07-03 morning: he runs
dhan + groww side by side and wants to SEE "which is extremely faster and more
precise", per feed, auto-extending to any future feed with zero portal changes.
The Latency "Measure now" panel today probes Dhan only and the instrument-load
view is Dhan-only — both must become per-feed and dynamic.

> **Guarantee matrices:** this plan cross-references the canonical 15-row +
> 7-row matrices in `.claude/rules/project/per-wave-guarantee-matrix.md`.
> Per-item specifics are in the Test Plan + Observability sections below.
> Scope is the operator-portal Lambda (Python) only — no Rust hot path, no
> QuestDB schema, no WebSocket scope change (2-WS lock untouched).

## Plan Items

- [x] 1. Per-feed dynamic latency measure in the Lambda `latency` action
  - Files: deploy/aws/lambda/operator-control/handler.py
  - Tests: test_parse_latency_per_feed_two_feeds, test_parse_latency_unknown_third_feed_row_appears,
    test_parse_latency_full, test_parse_latency_windowed_fields
  - Feed list discovered at measure time from the app's `GET /api/feeds/health`
    (curled twice on the box via SSM — T0 + T1 bracket the probe window).
    Never hardcoded. Per-feed TCP-connect + TLS-handshake ms probed on the box
    against a small feed→host map (`dhan` → api-feed.dhan.co, `groww` →
    socket-api.groww.in); probes run in PARALLEL background shell jobs with
    per-curl `--max-time` so one dead feed cannot hang the whole measure.
    A feed with no map entry still gets its row — network columns honestly
    read "endpoint unknown" (never fake numbers).
- [x] 2. Per-feed runtime metrics from the app's own reporting
  - Files: deploy/aws/lambda/operator-control/handler.py
  - Tests: test_ticks_per_sec_from_health_delta, test_ticks_per_sec_counter_reset_is_none
  - last-tick age + subscribed counts read verbatim from the health rows;
    ticks/sec computed as the delta of each feed's `ticks_total` between the
    two health scrapes over the measured window (None on counter reset /
    missing scrape — never fabricated).
- [x] 3. Comparison table + winner highlight + instrument-load line (portal JS)
  - Files: deploy/aws/lambda/operator-control/handler.py
  - Tests: test_feed_comparison_winners_lower_and_higher_better,
    test_feed_comparison_winners_tie_and_single_feed_no_winner,
    test_latency_card_iterates_feed_list_no_hardcoded_names
  - One row per feed; columns: TCP ms · TLS ms · last tick age · ticks/sec ·
    subscribed. Best value per column computed SERVER-SIDE (pure
    `_feed_comparison_winners`) and highlighted green; ties / single-feed /
    unknown values → no winner claim. Instrument-load line under the table:
    "dhan: N subscribed · groww: M subscribed" per feed. Existing QuestDB
    round-trip + clock-skew + per-tick processing cards KEPT and relabeled
    "box-wide" — the app's processing histograms carry NO feed label
    (verified: `crates/core/src/pipeline/tick_processor.rs:760` emits
    `tv_tick_processing_duration_ns` unlabeled), so per-feed processing
    splits are NOT faked.
- [x] 4. Tests + plan + archive
  - Files: deploy/aws/lambda/operator-control/test_handler.py,
    .claude/plans/active-plan-per-feed-latency.md,
    .claude/plans/archive/2026-07-03-destructive-interlock.md
  - Tests: full `python3 -m unittest discover deploy/aws/lambda/operator-control` green

## Design

The `latency` action's SSM script becomes a builder (`_latency_commands()`)
emitting one marked block per concern:

```
T0 → METRICS_T0 (histogram scrape) → FEEDS_T0 (/api/feeds/health)
→ per-feed TCP/TLS probes (parallel `( … ) &` + `wait`, per-curl --max-time)
→ QDB round-trip → chrony skew → sleep 4 (window floor)
→ T1 → METRICS (2nd scrape) → FEEDS_T1 (2nd health scrape)
```

`_parse_latency` (pure) gains: generic `PROBE_<feed>_BEGIN/END` block parsing,
`FEEDS_T0/T1` JSON extraction (reusing `_extract_marked_json`), a `feeds` list
(one entry per health row: probe ms + endpoint-known flag + last-tick age +
ticks/sec window delta + subscribed counts + verdict), and a `winners` map from
the pure `_feed_comparison_winners`. Box-wide fields (QuestDB RTT, skew,
windowed tick-processing percentiles) are unchanged. The portal JS renders the
table by ITERATING `j.feeds` — no feed name appears in the JS (ratcheted).

Worst-case SSM wall-clock stays under the 26s `_LATENCY_TIMEOUT_SECS`
(Lambda timeout 30s): 3 (metrics T0) + 4 (health T0) + 4 (parallel probes,
2 samples × 2s) + 3 (QDB) + 4 (sleep) + 3 (metrics T1) + 4 (health T1) ≈ 25s.

## Edge Cases

- App down on the box → both health curls emit `TV_CURL_FAILED` → `feeds` is
  empty + `feeds_error` names the cause; box-wide cards still render.
- Future feed #3 reported by the app but absent from the host map → row
  appears with app-side metrics; TCP/TLS cells read "endpoint unknown".
- Dead/blackholed feed endpoint → its probe curls fail per-sample within
  `--max-time 2` and print `x x` sentinels; the OTHER feed's probes run in a
  parallel job — total probe wall is bounded at samples × max-time.
- App restarted between the two health scrapes → ticks_total delta negative →
  ticks/sec = None (never a fabricated negative/huge rate).
- Only one feed has a value in a column, or best value tied → NO winner
  highlight (a comparison needs ≥2 real values and a strict best).
- Health JSON malformed → `_extract_marked_json` returns error; no row faked.
- T0 health scrape missing but T1 present → rows render, ticks/sec = None.

## Failure Modes

- SSM offline / box stopped → `_ssm_shell_sync` returns "" → existing
  empty-payload path: every field blank, UI shows "—" (no fake values).
- Probe hostname DNS-poisoned/unreachable → curl fails closed to `x x`
  sentinel; min() over positive samples only, blank when none.
- Lambda timeout pressure → budget documented in `_latency_commands()`;
  per-curl `--max-time` bounds every network leg; probes parallelized.
- A hostile feed name from the app (defense-in-depth) cannot reach the shell:
  probe commands are built ONLY from the static Python-side host map, never
  from the app's response; the app response is only parsed as JSON.

## Test Plan

Extend `deploy/aws/lambda/operator-control/test_handler.py` (stdlib unittest,
no boto3 needed for pure functions):
1. Two-feed fixture → 2 table rows with correct per-feed tcp/tls/age/tps/subscribed.
2. Fake 3rd feed ("zerodha") in the health fixture → 3rd row appears,
   `endpoint_known` false (endpoint-unknown handling), no probe values faked.
3. Winner logic: lower-better (tcp/tls/age) vs higher-better (tps/subscribed);
   tie → no winner; single comparable value → no winner.
4. ticks/sec: positive delta / window; counter reset → None.
5. No-hardcoded-feed-names ratchet: the latency JS iterates `j.feeds`; the
   loadLatency body contains no 'dhan'/'groww' literal.
6. Existing latency tests updated to the new marker format; full suite green:
   `python3 -m unittest discover deploy/aws/lambda/operator-control`.

## Rollback

Single-file revert of the handler.py + test_handler.py commit restores the
Dhan-only probe verbatim (the action name, auth, and SSM mechanism are
unchanged). No schema, no state, no infra change — the Lambda redeploys from
the previous artifact via the normal deploy workflow.

## Observability

- The measure result IS the observability artifact (operator-facing table).
- Honest labels: box-wide cards say "box-wide"; unknown endpoints say
  "endpoint unknown"; ticks/sec says it is measured over the probe window.
- Errors surface verbatim in the card (`feeds_error`), never silent defaults —
  consistent with the feeds card contract (audit Rule 11, no false OK).
- No new Prometheus metrics/alarms: this is a read-only operator view built
  from existing app metrics + health endpoints (charter §C monitoring rows
  N/A — no new failure mode is introduced on the box).

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | dhan + groww both live | 2 rows, real tcp/tls/age/tps/subscribed, best cells green |
| 2 | groww disabled | groww row still shown with its health verdict; tps 0/None |
| 3 | future feed #3 (no host map entry) | row appears, "endpoint unknown" network cells |
| 4 | app down | feeds_error rendered; box-wide cards still measured |
| 5 | box stopped | whole measure returns blanks ("—"), no fabrication |
| 6 | app restart mid-window | ticks/sec None, no negative rate |
| 7 | one feed endpoint blackholed | bounded by --max-time; other feed unaffected (parallel) |

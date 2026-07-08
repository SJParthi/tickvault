# Implementation Plan: QuestDB console shell-hang fix (B4 r3) + deploy-gate smoke

**Status:** VERIFIED
**Date:** 2026-07-06 (closure pass 2026-07-08)
**Approved by:** Parthiban (operator directive 2026-07-06 — console shell-hang fix scope grant; 2026-07-08 coordinator design ruling — the deploy gate is DELIBERATELY conservative and generic)
**Branch:** `claude/festive-bell-hm8w4d`
**Changed crates:** NONE (zero Rust — deploy/aws/lambda Python + workflows + terraform packaging excludes + evidence/harness files only)
**Evidence base:** `docs/incidents/2026-07-06-questdb-console-shell-hang/repro-evidence.md` (QuestDB 9.3.5 framing repro, 2026-07-06 — §9a/§9c/§10 cited throughout; relocated in the 2026-07-08 closure pass OUT of the lambda packaging dir so the terraform `archive_file` excludes need no special-casing; the provenance amendments inside the file record the round-7 commit, the round-8 §5 od-dump fill, the round-9 curl re-quoting, the round-10 reproducibility correction, and the closure relocation). The two probe scripts §9/§10 invoke are committed alongside as `repro_backlambda.py` + `raw_socket_probe.py` — both labelled RECONSTRUCTED (the session-ephemeral originals were never committed; the frozen OUTPUT blocks remain the raw evidence). Behavioral proof of the deploy gate = `scripts/questdb-console-gate-matrix.sh` (self-extracting, CI-wired — see FILE 8).

## Design ruling (binding, 2026-07-08 closure)

Twelve adversarial review rounds kept confirming new findings against the
deploy gate's per-(box-state x HTTP-code) diagnostics and against
overclaiming comments. The coordinator ruled: the gate is DELIBERATELY
conservative and generic. It does NOT diagnose per-state causes, does NOT
claim any code is "impossible" for any state, and accepts two DISCLOSED
limitations (see "Known limitations" below). The gate, harness, canary, and
this plan were REDUCED to that contract in the closure pass.

## Plan Items

- [x] FILE 1 — BACK lambda fix: L1 rewrite `GET /` → `GET /index.html` (after `_gate`, before the URL build); L2 `_NoFollowRedirect` opener + body-less 3xx relay in the `HTTPError` arm; `location` added to the success-path header tuple; the disproven r2 "closes the socket AFTER the body" claim removed; build marker → `b4-qdb-console-2026-07-06-r3`. The non-3xx error-body read is guarded so ANY exception there (socket timeout → `upstream_timeout`; ConnectionResetError / IncompleteRead / ConnectionAbortedError → `box_unreachable`) maps to the structured error envelope instead of escaping `lambda_handler` as a Lambda FunctionError. Honest bound in the comments: `_TIMEOUT_SECS=12` is PER blocking socket read, so total silence → `upstream_timeout` → front 504, while a slowly-dribbling unframed body is bounded by the back Lambda's 26s kill instead, surfacing as the front's 503/504 — degraded but bounded
  - Files: deploy/aws/lambda/questdb-console-proxy/handler.py
  - Tests: N/A-for-crates-grep (Python unittest in FILE 3; run via python3 -m unittest test_handler -v)
- [x] FILE 2 — FRONT lambda: build marker parity → `b4-qdb-console-2026-07-06-r3`; `location` added to the `_relay` forwarded-header tuple so a defense-in-depth relayed 3xx reaches the browser. Device-key auth, `/exec` handling, SQL gates, error mapping (incl. the 504 `upstream_timeout` text) byte-untouched
  - Files: deploy/aws/lambda/questdb-console-front/handler.py
  - Tests: N/A-for-crates-grep (Python unittest in FILE 4)
- [x] FILE 3 — BACK test suite: 10 new tests + 1 updated (test_root_get_is_rewritten_to_index_html; test_root_rewrite_preserves_whitelisted_query; test_head_root_not_rewritten; test_non_root_paths_not_rewritten; test_root_rewrite_kills_the_unframed_301_timeout_class; test_redirect_relayed_bodyless_never_reads_body; test_no_follow_redirect_opener_installed; test_disproven_r2_close_claim_removed; test_unframed_non_3xx_body_timeout_maps_to_upstream_timeout; test_non_timeout_error_body_read_failure_maps_to_box_unreachable; updated test_shell_get_forces_identity_and_connection_close). Full suite = 31 tests, all green (verbatim: "Ran 31 tests ... OK")
  - Files: deploy/aws/lambda/questdb-console-proxy/test_handler.py
  - Tests: N/A-for-crates-grep (the 11 Python tests named in this item's description; non-vacuity proven in the fix sessions by running the rewrite/3xx tests against patched-out r2 bytes — they FAIL — and the two error-body-read guard tests against pre-guard bytes where the exceptions ESCAPE lambda_handler)
- [x] FILE 4 — FRONT test suite: 1 new test (test_relay_forwards_location_on_3xx — FAILS on r2 because the front `_relay` header tuple drops `location`). No existing test modified. Full suite = 61 tests, all green (verbatim: "Ran 61 tests ... OK")
  - Files: deploy/aws/lambda/questdb-console-front/test_handler.py
  - Tests: N/A-for-crates-grep (test_relay_forwards_location_on_3xx)
- [x] FILE 5 — deploy gate in terraform-apply.yml, REWRITTEN in the closure pass as a minimal conservative decision tree (replacing the accumulated per-state diagnosis logic of rounds 1-12): (1) console disabled (TF var off) → one loud SKIP, exit 0; (2) `terraform output questdb_console_url` unresolvable → FATAL fail-closed, no deeper diagnosis; (3) SSM auth-token read failure → FATAL fail-closed; (4) up to 12 probes, 10s apart, each `curl -sS -o /dev/null -w '%{http_code}' --max-time 5 -H @"$HDR" "$QURL/"` — HTTP 200 → PASS immediately (the `--max-time 5` IS the 200-within-5s assertion; the pre-fix ~12s 504 can physically never pass), every code recorded; (5) after exhaustion, sample the EC2 box state ONCE: not running → loud SKIP exit 0 naming the state + codes seen (a not-running box cannot be probed; the gate re-verifies on the next in-window apply), running → FATAL exit 1 listing the codes seen and pointing triage at the console chain generically (SG :9000 ingress, back lambda, front lambda, QuestDB http) — NO impossibility claims, NO per-code attribution. Worst-case added wall clock: 12×5s probes + 11×10s waits ≈ 3 min. Secret hygiene: SSM read + `::add-mask::` + header FILE (umask 077 mktemp, trap-removed) — the bearer never appears in argv or logs. Behavioral proof: `scripts/questdb-console-gate-matrix.sh` (9 scenarios, one per decision-tree arm incl. the missing-gate-step extraction ABORT), run in CI on every PR via FILE 8
  - Files: .github/workflows/terraform-apply.yml, scripts/questdb-console-gate-matrix.sh
  - Tests: N/A-for-crates-grep (the workflow gate itself IS the runtime test; the 9-scenario harness pins its decision tree at merge time; YAML validated via python3 yaml.safe_load)
- [x] FILE 6 — box-local HARD canary in deploy-aws.yml (hard gate since the 2026-07-08 closure pass; earlier rounds shipped a WARN-only variant plus a Fetch-step grep/annotation block, both replaced): one line after the QUESTDB-UP check inside the `set -euo pipefail` SSM script — `curl -fsS --max-time 5 -o /dev/null http://127.0.0.1:9000/index.html || { echo FATAL...; exit 1; }`. Completion-within-5s is itself the framing assertion (an unframed keep-alive response never completes, hits `--max-time`, curl exits non-zero → deploy FAILS); `curl -f` fails any code ≥ 400. Disclosed limitation (stated in the workflow comment): a framed 3xx at /index.html (< 400) passes `curl -f` here — the end-to-end proxy gate (FILE 5) requires a literal 200 and catches that shape on the next in-window apply. Probing bare `/` is forbidden in this shell (the unframed keep-alive 301 would hang it)
  - Files: .github/workflows/deploy-aws.yml
  - Tests: N/A-for-crates-grep (the SSM script line itself is the gate; YAML validated; NOT ratcheted by any unit test/harness — see Known limitations)
- [x] FILE 7 — terraform: stale timeout comment corrected (the back handler's real 12s `_TIMEOUT_SECS`, per-socket-op, with the honest dribble bound naming the 26s Lambda backstop) + `archive_file` excludes for both console lambdas = `["test_handler.py", "README.md", "**/__pycache__/**"]` (the unit tests generate gitignored bytecode inside the source_dirs; the doublestar glob is legal per the hashicorp/archive provider docs). The excludes need NO evidence/harness special-casing because those files live outside the packaging dirs (see FILE 9)
  - Files: deploy/aws/terraform/questdb-console.tf
  - Tests: N/A-for-crates-grep (terraform fmt: EXPECTED clean but NOT RUN — no terraform binary in the fix sandbox; charter §4: stated plainly rather than asserted; a miss fails loudly pre-apply via the workflow's own fmt-check-recursive step)
- [x] FILE 8 — CI merge-time enforcement: the ci.yml `repo-guards` step "QuestDB console lambda unit tests (back + front)" (a step in an existing job that feeds All Green — merge-gate-lock §5) runs both lambda unittest suites AND `bash scripts/questdb-console-gate-matrix.sh` on every PR. The harness self-extracts the gate step from terraform-apply.yml and ABORTS loudly ("gate step not found") if the step is deleted or renamed
  - Files: .github/workflows/ci.yml
  - Tests: N/A-for-crates-grep (the CI step itself executes the FILE 3 + FILE 4 suites and the 9-scenario harness at merge time; YAML validated via python3 yaml.safe_load)
- [x] FILE 9 — evidence hygiene (closure pass): `repro-evidence.md` relocated to `docs/incidents/2026-07-06-questdb-console-shell-hang/` (outside every lambda packaging dir); the §9/§10 probe scripts committed alongside as `repro_backlambda.py` + `raw_socket_probe.py`, both explicitly labelled RECONSTRUCTED in their docstrings (the originals were session-ephemeral; the frozen OUTPUT blocks remain the raw evidence); the old in-lambda-dir `gate-matrix-r7.sh` deleted and replaced by `scripts/questdb-console-gate-matrix.sh`. No committed file cites a scratchpad path as evidence
  - Files: docs/incidents/2026-07-06-questdb-console-shell-hang/repro-evidence.md, docs/incidents/2026-07-06-questdb-console-shell-hang/repro_backlambda.py, docs/incidents/2026-07-06-questdb-console-shell-hang/raw_socket_probe.py
  - Tests: N/A-for-crates-grep (frozen evidence + labelled reconstructions; the harness in scripts/ is exercised by FILE 8)

## Design

**Root cause (Verified — docs/incidents/2026-07-06-questdb-console-shell-hang/repro-evidence.md):**
QuestDB 9.3.5 answers `GET /` with an UNFRAMED keep-alive 301 → `/index.html`
— raw bytes (§10):
`b'HTTP/1.1 301 Moved Permanently\r\nServer: questDB/1.0\r\n...Location: /index.html\r\n\r\n\r\n'`
— NO Content-Length, NO Transfer-Encoding, NO Connection header — and NEVER
closes the socket even under request `Connection: close` ("recv TIMED OUT
after 20.017s gap — server NEVER closed the socket"). urllib's default
`HTTPRedirectHandler` drains that unframed body with `fp.read()` BEFORE
following, so the back lambda's `urlopen` itself blocks until
`_TIMEOUT_SECS=12` (§9a: TimeoutError at t+12.024s) → `upstream_timeout` →
front 504. The deployed r2 fix's premise ("QuestDB closes the socket AFTER
the body") is disproven verbatim. `/index.html` is Content-Length-framed
(765 B): the byte-identical relay returns 200 in 0.004s (§9c). `/exec` is
chunked (§1) — untouched and unaffected. GET / had NEVER returned 200 in the
console's lifetime.

**Two independent handler layers, each individually sufficient:**

- **L1 (primary):** the back lambda rewrites `GET /` → `GET /index.html`
  AFTER gating, BEFORE the URL build — the unframed 301 is never elicited.
  GET-only: `HEAD /` stays on its proven framed-405 path (§8);
  `HEAD /index.html` is live-unverified, deliberately unchanged. Placement
  after `_gate` is deliberate: both "/" and "/index.html" are whitelisted
  static, the rewrite target is a constant, and the rewrite can never widen
  the whitelist. The browser URL stays "/"; the shell's RELATIVE asset refs
  resolve identically, and the per-release hashed asset refs come from the
  shell itself, so they track the deployed QuestDB version. The
  `/index.html` LOCATION is a drift surface — see Known limitations.
- **L2 (belt, class-wide):** a `_NoFollowRedirect` opener
  (`redirect_request` returns None — CPython's `http_error_30x` then raises
  `HTTPError` WITHOUT the `fp.read()` drain) + a body-less 3xx relay arm —
  if ANY whitelisted path ever 3xxs unframed in a future QuestDB, it relays
  instantly (status + Location, body never read) instead of hanging. The
  front forwards `location` and the browser follows. The delimiter-less `/`
  301 is the only delimiter-less response OBSERVED in our 2026-07-06 test
  matrix (/, /index.html, one /assets/ file, /exec, HEAD /) — no claim is
  made about unprobed paths; an unframed NON-3xx on an unprobed path is
  guarded inside the HTTPError arm (see Failure Modes).

**Deploy verification (two layers, both generic):**

- The FILE 5 terraform-apply gate asserts GET / == 200 within 5s per attempt
  through the WHOLE proxy chain, with the conservative decision tree in the
  item description. It runs immediately BEFORE the "Publish QuestDB console
  URL to Telegram" step; on a PASS the console is verified before the
  announce; on any exit-0 SKIP (disabled, box not running) the publish still
  runs and the URL is announced UNVERIFIED, with the skip reason in the run
  log/step summary — an accepted, disclosed window (see Known limitations).
- The FILE 6 deploy-aws box-local HARD canary fails the box deploy that
  ships a QuestDB image whose /index.html no longer serves as a complete
  200 within 5s.

## Edge Cases

- **Query string on `/`** — preserved by the rewrite (`?v=1` →
  `/index.html?v=1`); pinned by test_root_rewrite_preserves_whitelisted_query.
- **`HEAD /`** — NOT rewritten (rewrite is GET-gated); QuestDB answers a
  FRAMED chunked 405 in 1.2ms (§8); pinned by test_head_root_not_rewritten.
- **3xx on non-`/` paths** (`/chk`, `/settings`, future assets) — relayed
  body-less in ~ms via L2 instead of a 12s hang; pinned by
  test_redirect_relayed_bodyless_never_reads_body (a bomb-fp asserts the
  body is NEVER read).
- **Box not running during the smoke gate** (nights, weekends, the
  08:30–16:30 IST auto start/stop schedule) — after the probe window the
  gate samples the EC2 state once; a non-running state SKIPs loudly with the
  state + every observed code printed. No claim is made about which HTTP
  codes a given box state can or cannot produce — the gate simply cannot
  verify a box that is not steadily running (disclosed limitation).
- **Lambda cold start / QuestDB start-restart windows vs the 5s budget** —
  12 probes spread over ~2–3 minutes absorb front + VPC-back cold starts and
  ordinary container start/restart windows; a single 200 anywhere in the
  window passes. A box whose OS/QuestDB takes longer than the whole window
  to come up while EC2 already reports `running` still FATALs — bounded and
  disclosed (fail-closed is preferred over laundering a real regression).
- **Future QuestDB renames `/index.html`** — the box-deploy lane
  (deploy/docker/**) does not trigger terraform-apply.yml, so the FILE 5
  gate does not run on the deploy that ships an image bump; the FILE 6
  box-local HARD canary fails that deploy instead (completion-within-5s +
  `-f`), with the framed-3xx shape disclosed as passing the canary and being
  caught by the FILE 5 gate's literal-200 requirement on the next in-window
  apply.

## Failure Modes

- **Genuinely slow box** — still an honest 504 (`upstream_timeout` mapping
  UNCHANGED) — and post-fix that 504 finally MEANS slow, not "unframed 301".
- **Dead/stopped box** — no static shell exists that could fake a 200 on a
  dead box; the FILE 5 gate SKIPs loudly (state + codes printed) rather than
  guessing causes.
- **Unframed NON-3xx on a future path** — explicitly GUARDED inside the back
  lambda's HTTPError arm. HONEST BOUND: `_TIMEOUT_SECS=12` is PER blocking
  socket read (`_read_capped` issues up to ~16 sequential capped `read()`
  calls — ceil(4,100,000 / 262,144) counts `fp.read()` CALLS, not recvs;
  http.client's BufferedReader loops raw recvs inside each read(), every
  recv resetting the 12s timer). TOTAL SILENCE → guarded `upstream_timeout`
  → front 504; a DRIBBLING body (≥1 byte per <12s read) never times a read
  out and is bounded by the back Lambda's 26s kill instead → Lambda
  FunctionError → the front's 503 — degraded but bounded. A socket timeout
  raised while reading the error body INSIDE the `except HTTPError` handler
  escapes the enclosing try's sibling clauses (Python semantics), so the
  read is guarded explicitly: timeout → `upstream_timeout`; any other
  exception (ConnectionResetError — the QuestDB container restart on every
  box deploy — IncompleteRead, ConnectionAbortedError) → typed
  `box_unreachable`. Both pinned by
  test_unframed_non_3xx_body_timeout_maps_to_upstream_timeout +
  test_non_timeout_error_body_read_failure_maps_to_box_unreachable.
- **Smoke gate cannot resolve the terraform output / read the SSM secret** —
  FATAL fail-closed with a plain message (no deeper diagnosis); pinned by
  harness scenarios B + C.
- **Console disabled in terraform** — the gate SKIPs loudly; pinned by
  harness scenario A.

## Known limitations / not covered (disclosed, design-accepted)

1. **Off-window applies SKIP console verification.** An apply while the box
   is not steadily running cannot verify the console; the gate SKIPs loudly
   (state + codes printed) and the URL publish still runs, so a regression
   shipped by such an apply surfaces at the next in-window apply or the next
   box-deploy canary — not on that apply.
2. **QuestDB-version shell drift is caught at the NEXT box deploy** by the
   FILE 6 box-local hard canary (the image-bump lane does not trigger the
   FILE 5 gate), not same-instant on terraform applies between deploys. A
   framed 3xx at /index.html passes the box canary (`curl -f` < 400) and is
   caught by the FILE 5 gate's literal-200 requirement on the next in-window
   apply.
3. **Deliberately un-ratcheted layers:** (a) a `continue-on-error:` added to
   the FILE 5 gate step (the harness executes the step's `run` script, not
   its YAML attributes); (b) the FILE 8 ci.yml step's own existence
   (deleting it un-wires the suites + harness silently); (c) the FILE 6
   deploy-aws hard-canary line (no unit test or harness pins it — the
   harness extracts exclusively from terraform-apply.yml); (d) the exact
   wording of the gate's SKIP/FATAL messages beyond the phrases the 9
   harness scenarios grep.
4. **Dribbling-unframed-body worst case** through the back lambda = bounded
   by the 26s Lambda kill → the front's 503/504 — degraded but bounded,
   never an unbounded hang (see Failure Modes).

## Test Plan

- `cd deploy/aws/lambda/questdb-console-proxy && python3 -m unittest test_handler -v`
  — 31 tests, OK (counts recorded in the closure commit message;
  re-runnable via the command above).
- `cd deploy/aws/lambda/questdb-console-front && python3 -m unittest test_handler -v`
  — 61 tests, OK.
- `bash scripts/questdb-console-gate-matrix.sh` — 9 scenarios, one per gate
  decision-tree arm (disabled-skip; tf-output-failure FATAL; ssm-failure
  FATAL; 200-first-try PASS; 200-after-retries PASS; exhausted+running
  FATAL; exhausted+stopped SKIP; exhausted+stopping SKIP; missing-gate-step
  extraction ABORT) — "gate-matrix: all 9 scenarios green" (counts recorded
  in the closure commit message; re-runnable via the command above). The
  harness ABORTS
  with one clear error if the gate step cannot be extracted and trap-cleans
  its mktemp work tree.
- **Non-vacuity (from the fix sessions):** the rewrite/3xx handler tests
  FAIL against patched-out r2 bytes; the two error-body-read guard tests
  FAIL against pre-guard bytes (the exceptions ESCAPE lambda_handler).
- **FILE 5 gate red-on-pre-fix / green-post-fix:** pre-fix GET / = 504 after
  ~12s can never pass `--max-time 5`; post-fix §9c measured 4ms box-side.
- **CI merge-time enforcement:** both suites + the harness run on every PR
  via the ci.yml Repo Guards step "QuestDB console lambda unit tests
  (back + front)" (FILE 8), which feeds All Green.
- Workflow YAML: `python3 -c "import yaml; yaml.safe_load(...)"` on all
  edited workflows (terraform-apply.yml, deploy-aws.yml, ci.yml).
  `terraform fmt -check`: EXPECTED clean but NOT RUN — no terraform binary
  in the fix sandbox (charter §4: stated plainly, not asserted); the
  workflow's own `terraform fmt -check -recursive` step fails loudly
  pre-apply on any miss.

## Rollback

A single `git revert` of the squash commit restores today's exact state: the
handler rewrite + opener + comments, both build markers, the tests, the
workflow steps, the TF excludes/comment, the evidence relocation, and this
plan. No terraform resources/state, no schema, no secrets change; the next
terraform-apply repackages the reverted bytes via `source_code_hash`. Honest
note: a revert re-opens the incident and the FILE 5 gate then goes red on
the next in-window apply — that is the desired ratchet; a conscious revert
must pair with removing (or accepting) the red gate.

## Observability

- The front's existing structured `_log` lines now show `outcome=ok
  status=200` for path "/" (previously `back_error status=504`).
- The smoke-gate verdict lands in the Actions log AND `GITHUB_STEP_SUMMARY`
  (✅ pass / ⚠ loud skip with state + codes / FATAL with the codes seen —
  the token is masked + header-file, never argv).
- The box canary either passes silently or FAILS the deploy with a FATAL
  line in the SSM output the deploy workflow already surfaces.
- Build marker `b4-qdb-console-2026-07-06-r3` in BOTH lambdas proves the
  deployed bytes (existing proof-3 ratchet: test_build_marker_present).

## Per-Item Guarantee Matrix

Cross-reference: `.claude/rules/project/per-wave-guarantee-matrix.md` (the
15-row + 7-row matrices apply per that rule; rows below state how each lands
for THIS zero-Rust deploy-layer change, with honest N/A where the dimension
is Rust-only):

| Dimension | This plan |
|---|---|
| 100% code coverage (llvm-cov ratchet) | N/A — Python lambda, no llvm-cov surface; covered by the 12 new/updated unit tests (FILE 3: 10 new + 1 updated back; FILE 4: 1 new front) inside the 31-test back + 61-test front suites, CI-enforced at merge time via the ci.yml Repo Guards step (FILE 8) |
| DHAT / Criterion / hot-path | N/A — cold-path observability lambda, zero Rust, zero hot-path code |
| Audit table + DEDUP keys | N/A — no SEBI-relevant event, no DB write; forensic record = Actions log + step summary + the front's structured `_log` |
| 100% logging | front `_log` structured JSON per request (unchanged, now shows ok/200 on "/") |
| 100% alerting | the FILE 5 deploy gate red-fails an in-window apply on any probeable shell regression; the FILE 6 hard canary red-fails the box deploy that ships a broken shell. The gate's decision tree is pinned at merge time by the 9-scenario harness (FILE 8, feeds All Green). Honestly NOT covered: the items in "Known limitations" §1–§3 |
| 100% security | secret hygiene: `::add-mask::` + header-FILE (umask 077 mktemp) + trap rm; device-key auth byte-untouched; SQL gates byte-untouched; /exec untouched |
| 100% scenarios | the 9-scenario gate matrix + the 92 lambda unit tests; each Design/Edge-Case claim cites its evidence section |
| 100% code review | twelve adversarial review rounds + the 2026-07-08 closure reduction per the coordinator's design ruling |
| Zero ticks lost / WS / QuestDB resilience rows | N/A impact — nothing in the tick path, WS path, or QuestDB write path is touched; the console is a read-only observability surface |

Archive this plan to `.claude/plans/archive/2026-07-06-console-shell-hang-fix.md`
after push per `plan-enforcement.md`.

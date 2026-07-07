# Implementation Plan: QuestDB console shell-hang fix (B4 r3) + deploy-gate smoke

**Status:** VERIFIED
**Date:** 2026-07-06
**Approved by:** Parthiban (operator directive 2026-07-06 — console shell-hang fix scope grant)
**Branch:** `claude/festive-bell-hm8w4d`
**Changed crates:** NONE (zero Rust — deploy/aws/lambda Python + workflows + one terraform comment only)
**Evidence base:** `scratchpad/repro-evidence.md` (QuestDB 9.3.5 framing repro, 2026-07-06 — §9a/§9c/§10 cited throughout)

## Plan Items

- [x] FILE 1 — BACK lambda fix: L1 rewrite `GET /` → `GET /index.html` (after `_gate`, before URL build), L2 `_NoFollowRedirect` opener + body-less 3xx relay in the `HTTPError` arm, `location` added to the success-path header tuple, r2 comment block rewritten (the disproven "closes the socket AFTER the body" claim removed), build marker → `b4-qdb-console-2026-07-06-r3`. Fixer round 2 (2026-07-06): explicit timeout guard around the non-3xx error-body read. Fixer round 3 (2026-07-06): sibling `except Exception → box_unreachable` on the SAME read (the round-2 guard covered only the timeout member of its stated escape class — ConnectionResetError / IncompleteRead / ConnectionAbortedError still escaped as a Lambda FunctionError, executed proof), plus per-recv-honest comment wording (see the Failure Modes bullet)
  - Files: deploy/aws/lambda/questdb-console-proxy/handler.py
  - Tests: N/A-for-crates-grep (Python unittest in FILE 3; run via python3 -m unittest test_handler -v)
- [x] FILE 2 — FRONT lambda: build marker parity → `b4-qdb-console-2026-07-06-r3`; `location` added to the `_relay` forwarded-header tuple so a defense-in-depth relayed 3xx reaches the browser. Device-key auth, `/exec` handling, SQL gates, error mapping (incl. the 504 `upstream_timeout` text) byte-untouched
  - Files: deploy/aws/lambda/questdb-console-front/handler.py
  - Tests: N/A-for-crates-grep (Python unittest in FILE 4)
- [x] FILE 3 — BACK test suite: 10 new tests + 1 updated (test_root_get_is_rewritten_to_index_html; test_root_rewrite_preserves_whitelisted_query; test_head_root_not_rewritten; test_non_root_paths_not_rewritten; test_root_rewrite_kills_the_unframed_301_timeout_class; test_redirect_relayed_bodyless_never_reads_body; test_no_follow_redirect_opener_installed; test_disproven_r2_close_claim_removed; test_unframed_non_3xx_body_timeout_maps_to_upstream_timeout [fixer round 2]; test_non_timeout_error_body_read_failure_maps_to_box_unreachable [fixer round 3]; updated test_shell_get_forces_identity_and_connection_close)
  - Files: deploy/aws/lambda/questdb-console-proxy/test_handler.py
  - Tests: N/A-for-crates-grep (the 11 Python tests named in this item's description; non-vacuity proven by running tests 1 + 6 against patched-out r2 bytes — they FAIL — and the two error-body-read guard tests against pre-guard bytes where the exceptions ESCAPE lambda_handler)
- [x] FILE 4 — FRONT test suite: 1 new test (test_relay_forwards_location_on_3xx — FAILS on r2 because the front `_relay` header tuple drops `location`). No existing test modified; the 504 `upstream_timeout` mapping test stays as-is
  - Files: deploy/aws/lambda/questdb-console-front/test_handler.py
  - Tests: N/A-for-crates-grep (test_relay_forwards_location_on_3xx)
- [x] FILE 5 — HARD deploy gate in terraform-apply.yml: new step "Deploy gate: console shell GET / must return 200 within 5s" inserted BEFORE "Publish QuestDB console URL to Telegram" (gate first, announce after). 3 attempts each hard-capped at 5s; body must contain the console shell HTML; 000/504/503 forgiven ONLY for a verifiably not-running box (terraform `instance_id` output → ec2 describe-instances; fixer round 1 2026-07-06: a STOPPED box manifests as CODE=000 through this gate — its ENI drops packets with no RST, so the back lambda's connect blocks the full 12s `_TIMEOUT_SECS` → upstream_timeout → front 504 at ~12s while the gate curl caps at 5s; a fast 503 needs a REFUSAL = box running with QuestDB down); unknown state FAILS CLOSED; a transient `terraform output questdb_console_url` failure also FAILS CLOSED (only the null/not-found console-disabled shape skips); fixer round 5 2026-07-06: running+503 gets ONE 90s grace re-probe before FATAL (a fast 503 on a RUNNING box is also the transient signature of QuestDB not yet listening — the 08:30 IST box auto-start window and the 15:46 IST `docker compose up -d` container-recreate window — and a grace pass still requires a genuine 200 + shell HTML, never a skip; 000/504 = the shell-hang class stay immediately FATAL). Secret hygiene: SSM read + ::add-mask:: + header-FILE (umask 077 mktemp), token never in argv
  - Files: .github/workflows/terraform-apply.yml
  - Tests: N/A-for-crates-grep (the workflow gate itself IS the test; YAML validated via python3 yaml.safe_load; pre-fix bytes physically cannot pass — GET / needs ~12s while the per-attempt cap is 5s)
- [x] FILE 6 — box-local canary in deploy-aws.yml: two SSM command-array elements after the QUESTDB-UP loop, before the TRADING APP FIRST block — NON-FATAL loud WARN probing the rewrite TARGET `/index.html` (never bare `/`, which would hang the SSM shell) so a QuestDB image bump that breaks the shell is surfaced LOUDLY on the deploy that ships it — fixer round 1 (2026-07-06): the Fetch step now captures the SSM stdout to a file and greps for the canary WARN, emitting a job-level `::warning::` annotation + a `$GITHUB_STEP_SUMMARY` line (and a separate UNKNOWN warning when neither the WARN nor `QDB-SHELL-CANARY-OK` marker is present, e.g. SSM 24,000-char truncation). Non-fatal is mandated by deploy-aws.yml's own TRADING-APP-FIRST charter (a broken observability component must never block the trading deploy)
  - Files: .github/workflows/deploy-aws.yml
  - Tests: N/A-for-crates-grep (echoes QDB-SHELL-CANARY-OK or a WARN line in the SSM output the workflow already surfaces; YAML validated)
- [x] FILE 7 — terraform comment truth-correction ONLY (the stale "25s" claim → the back handler's real 12s `_TIMEOUT_SECS`; fixer round 4 also relabels "~16 sequential recvs" → "~16 sequential capped read() calls, each of which may span MANY recvs" — the ~16 counts `fp.read()` CALLS in `_read_capped`, ceil(4,100,000 / 262,144), not socket recvs; http.client's BufferedReader loops raw recvs per read() and each recv resets the 12s timer, so under a dribble the recv count is bounded only by the Lambda 26s kill). Zero infra diff, and NOT deploy-load-bearing (fixer round 4 correction — the earlier "LOAD-BEARING … makes the r3 lambdas deploy on the merge push" claim was wrong on BOTH halves, Verified: (1) this PR edits `.github/workflows/terraform-apply.yml` itself, which is IN the workflow's own push path filter (`paths: - 'deploy/aws/terraform/**' - '.github/workflows/terraform-apply.yml'`), so the merge push triggers the workflow with or without this tf touch; (2) the apply fires on `terraform plan -detailed-exitcode` = 2, which the changed lambda handler bytes already guarantee via `data.archive_file.questdb_console_proxy[0].output_base64sha256` → `source_code_hash` drift — no .tf edit is needed to produce drift. Nuance: the push-triggered apply is still market-hours-guarded, so a mid-market merge deploys via the 15:50 IST scheduled slot, not literally "on the merge push". Do NOT copy a "touch a tf comment to force deploy" pattern from this item — it is a non-mechanism)
  - Files: deploy/aws/terraform/questdb-console.tf
  - Tests: N/A-for-crates-grep (terraform fmt -check stays clean; comment-only)
- [x] FILE 8 — CI merge-time enforcement of the lambda test suites (fixer round 4, 2026-07-06): new step "QuestDB console lambda unit tests (back + front)" appended to the EXISTING ci.yml `repo-guards` job (already in All Green's `needs:` — a step in an existing needed job, never a new job, per merge-gate-lock §5). Runs `python3 -m unittest test_handler -v` in both console lambda dirs unconditionally (stdlib-only suites, <1s). Closes the adversarially-confirmed gap that NO CI lane executed these suites — a PR partially reverting the r3 handler fix previously merged with every check green, caught only by the FILE 5 runtime gate on the next terraform-drift apply
  - Files: .github/workflows/ci.yml
  - Tests: N/A-for-crates-grep (the CI step itself executes the FILE 3 + FILE 4 suites at merge time; YAML validated via python3 yaml.safe_load)

## Design

**Root cause (Verified — scratchpad/repro-evidence.md):** QuestDB 9.3.5 answers
`GET /` with an UNFRAMED keep-alive 301 → `/index.html` — raw bytes (§10):
`b'HTTP/1.1 301 Moved Permanently\r\nServer: questDB/1.0\r\n...Location: /index.html\r\n\r\n\r\n'`
— NO Content-Length, NO Transfer-Encoding, NO Connection header — and NEVER
closes the socket even under request `Connection: close` ("recv TIMED OUT after
20.017s gap — server NEVER closed the socket"). urllib's default
`HTTPRedirectHandler` drains that unframed body with `fp.read()` BEFORE
following, so the back lambda's `urlopen` itself blocks until
`_TIMEOUT_SECS=12` (§9a: TimeoutError at t+12.024s) → `upstream_timeout` →
front 504. The deployed r2 fix's premise ("QuestDB closes the socket AFTER the
body") is disproven verbatim. `/index.html` is Content-Length-framed (765 B):
the byte-identical relay returns 200 in 0.004s (§9c). `/exec` is chunked (§1)
— untouched and unaffected. GET / had NEVER returned 200 in the console's
lifetime.

**Two independent layers, each individually sufficient:**

- **L1 (primary):** the back lambda rewrites `GET /` → `GET /index.html` AFTER
  gating, BEFORE the URL build — the unframed 301 is never elicited. GET-only:
  `HEAD /` stays on its proven framed-405 path (§8); `HEAD /index.html` is
  live-unverified, deliberately unchanged. Placement after `_gate` is
  deliberate: both "/" and "/index.html" are whitelisted static
  (`_classify_path`), the rewrite target is a constant, and the rewrite can
  never widen the whitelist. The browser URL stays "/"; the shell's RELATIVE
  asset refs resolve identically, and `/index.html` itself carries the
  per-release hashed asset refs so nothing drifts on a QuestDB upgrade.
- **L2 (belt, class-wide):** a `_NoFollowRedirect` opener
  (`redirect_request` returns None — CPython's `http_error_30x` then raises
  `HTTPError` WITHOUT the `fp.read()` drain) + a body-less 3xx relay arm — if
  ANY whitelisted path (`/chk`, `/settings`, future assets) ever 3xxs unframed
  in a future QuestDB, it relays instantly (status + Location, body never
  read) instead of hanging. The front forwards `location` and the browser
  follows. `install_opener` keeps the call site as plain
  `urllib.request.urlopen`, so every existing `mock.patch("urllib.request.urlopen")`
  test passes unchanged.

**Gate-before-announce:** the FILE 5 hard gate runs immediately BEFORE the
"Publish QuestDB console URL to Telegram" step, so a broken console is never
announced as live. Each of its 3 attempts is hard-capped at 5s (`--max-time 5`),
so the pre-fix behavior (504 after ~12s) can physically never pass.

## Edge Cases

- **Query string on `/`** — preserved by the rewrite (`?v=1` →
  `/index.html?v=1`); pinned by test_root_rewrite_preserves_whitelisted_query.
- **`HEAD /`** — NOT rewritten (rewrite is GET-gated); QuestDB answers a FRAMED
  chunked 405 in 1.2ms (§8); pinned by test_head_root_not_rewritten.
- **3xx on non-`/` paths** (`/chk`, `/settings`, future assets) — relayed
  body-less in ~ms via L2 instead of a 12s hang; pinned by
  test_redirect_relayed_bodyless_never_reads_body (a bomb-fp asserts the body
  is NEVER read).
- **Box auto-stopped during the smoke gate** — the gate verifies the EC2 state
  via the terraform `instance_id` output; 000/504/503 with the box verifiably
  stopped/stopping/pending SKIPS LOUDLY (auto-stop window is the console
  working correctly — a stopped box shows as 000 through the 5s-capped curl,
  never a fast 503, because its ENI drops packets and the back lambda's
  connect blocks the full 12s socket timeout); any of those codes with the
  box RUNNING or an UNKNOWN state FAILS CLOSED (audit Rule 11: no false-OK).
- **Lambda cold start vs the 5s budget** — 3 attempts with 3s sleeps absorb
  front + VPC-back cold starts; EACH attempt stays capped at 5s so the
  per-request contract holds.
- **Future QuestDB renames `/index.html`** — HONEST ENVELOPE (fixer round 1,
  2026-07-06): the deploy lane that SHIPS a QuestDB image bump is
  deploy-aws.yml (`deploy/docker/**`), which does NOT trigger
  terraform-apply.yml (path-filtered to `deploy/aws/terraform/**` + its own
  file) and produces zero terraform drift — so the FILE 5 hard gate does NOT
  run on that deploy; it FATALs on the NEXT terraform-drift apply (which can
  be days later). Same-day protection on the shipping lane is the FILE 6 box
  canary, now surfaced as a job-level `::warning::` + step-summary line by
  the deploy-aws Fetch step (non-fatal per TRADING-APP-FIRST — a console
  fault never blocks the trading deploy).

## Failure Modes

- **Genuinely slow box** — still an honest 504 (`upstream_timeout` mapping
  UNCHANGED — back handler timeout arms + the front's 504 mapping) — and
  post-fix that 504 finally MEANS slow, not "unframed 301".
- **Dead box** — no static shell exists that could fake a 200 on a dead box.
  Precise codes (fixer round 1, 2026-07-06): a STOPPED box drops packets (no
  RST), so the console answers 504 `upstream_timeout` after the back lambda's
  12s connect timeout — the front's 503 `box_unreachable` needs a connection
  REFUSAL (box running, QuestDB down). The smoke gate independently verifies
  the EC2 state before accepting ANY of 000/504/503 as the auto-stop window.
- **Unframed NON-3xx on a future path** — explicitly GUARDED inside the back
  lambda's HTTPError arm (fixer rounds 2+3, 2026-07-06). HONEST BOUND (fixer
  round 3 — the earlier "bounded 12s read → honest upstream_timeout → front
  504, never a silent hang" wording overstated it; it holds only for total
  silence): `_TIMEOUT_SECS=12` is PER socket recv (`_read_capped` issues up
  to ~16 sequential capped `read()` calls — ceil(MAX_BODY_BYTES 4,100,000 /
  _READ_CHUNK 262,144) counts `fp.read()` CALLS, not recvs; http.client's
  BufferedReader loops raw socket recvs inside each read(), every recv
  resetting the 12s timer, so under a dribble the recv count is bounded only
  by the Lambda 26s kill — same honest bound the questdb-console.tf
  `timeout = 26` comment states). TOTAL SILENCE
  → guarded `upstream_timeout` → front 504; a DRIBBLING body (≥1 byte per
  <12s recv) never times a recv out and is bounded by the back Lambda's 26s
  timeout instead → Lambda FunctionError → the front's generic 503 (a
  hard-timeout kill cannot be mapped in-process). Round-2 honesty
  correction retained: a socket timeout raised while reading the error body
  INSIDE the `except HTTPError` handler was NOT caught by the sibling
  timeout clause (an exception raised in an except suite escapes the whole
  try statement) — it escaped `lambda_handler` as a Lambda FunctionError,
  which the front mapped to a dishonest offline-503; proven by execution and
  pinned by `test_unframed_non_3xx_body_timeout_maps_to_upstream_timeout`.
  Fixer round 3 closed the NON-timeout members of the same escape class:
  ConnectionResetError (peer RST mid error body — the QuestDB container
  restart on every box deploy), http.client.IncompleteRead, and
  ConnectionAbortedError also escaped as a FunctionError (executed proof:
  `ESCAPED lambda_handler: ConnectionResetError: peer RST mid error body`);
  a sibling `except Exception → box_unreachable` now mirrors the success
  path's blanket arm, pinned by
  `test_non_timeout_error_body_read_failure_maps_to_box_unreachable`.
  Read-until-idle was explicitly rejected in the repro design notes (it
  would turn every load into a deliberate multi-second stall).
- **Smoke gate cannot read the SSM secret / cannot resolve box state** — fails
  closed (the `aws ssm get-parameter` failure aborts under `set -euo
  pipefail`; unknown EC2 state is an explicit FATAL arm).
- **Console disabled in terraform** — the gate skips loudly ("nothing deployed
  to gate"), mirroring the publish step's own disabled arm. Fixer round 1
  (2026-07-06): ONLY the known disabled shape (terraform output null / not
  found) skips; any other `terraform output` failure (state lock, backend
  hiccup) FAILS CLOSED instead of masquerading as disabled (audit Rule 11).

## Test Plan

- `cd deploy/aws/lambda/questdb-console-proxy && python3 -m unittest test_handler -v`
  — all pass incl. the 10 new tests (8 from r3 + 1 fixer round 2 + 1 fixer
  round 3); the `OK` line is pasted in the PR body (charter §4 evidence
  discipline).
- `cd deploy/aws/lambda/questdb-console-front && python3 -m unittest test_handler -v`
  — all pass incl. test_relay_forwards_location_on_3xx.
- **Non-vacuity:** tests 1 (test_root_get_is_rewritten_to_index_html) and 6
  (test_redirect_relayed_bodyless_never_reads_body) are re-run against
  patched-out r2 handler bytes and FAIL there (evidence pasted in the PR
  body). Test 5 (test_root_rewrite_kills_the_unframed_301_timeout_class)
  behaviorally reproduces the prod 504 on r2 bytes.
- **FILE 5 gate red-on-pre-fix / green-post-fix:** pre-fix GET / = 504 after
  ~12s can never pass `--max-time 5`; post-fix §9c measured 4ms box-side.
- **CI merge-time enforcement (fixer round 4):** both suites also run on
  every PR via the ci.yml Repo Guards step "QuestDB console lambda unit
  tests (back + front)" (FILE 8), which feeds All Green — a red suite now
  blocks the merge instead of waiting for the FILE 5 runtime gate on the
  next terraform-drift apply.
- Workflow YAML: `python3 -c "import yaml; yaml.safe_load(...)"` on all
  edited workflows (incl. ci.yml). `terraform -chdir=deploy/aws/terraform
  fmt -check` stays clean (comment-only change).

## Rollback

A single `git revert` of the squash commit restores today's exact state: the
handler rewrite + opener + comments, both build markers, the tests, both
workflow steps, the TF comment, and this plan. No terraform resources/state,
no schema, no secrets change; the next terraform-apply repackages the reverted
bytes via `source_code_hash`. Honest note: a revert re-opens the incident and
the FILE 5 gate then goes red on the next apply — that is the desired ratchet;
a conscious revert must pair with removing (or accepting) the red gate.

## Observability

- The front's existing structured `_log` lines now show `outcome=ok status=200`
  for path "/" (previously `back_error status=504`).
- The smoke-gate verdict lands in the Actions log AND `GITHUB_STEP_SUMMARY`
  (✅ pass / ⚠ loud skip / FATAL with the observed code + body head — the body
  head is audit-safe: front error JSON / login HTML; the token is masked +
  header-file, never argv).
- The box canary echoes `QDB-SHELL-CANARY-OK` / a WARN line in the SSM command
  output the deploy workflow already surfaces.
- Build marker `b4-qdb-console-2026-07-06-r3` in BOTH lambdas proves the
  deployed bytes (existing proof-3 ratchet: test_build_marker_present).

## Per-Item Guarantee Matrix

Cross-reference: `.claude/rules/project/per-wave-guarantee-matrix.md` (the
15-row + 7-row matrices apply per that rule; rows below state how each lands
for THIS zero-Rust deploy-layer change, with honest N/A where the dimension is
Rust-only):

| Dimension | This plan |
|---|---|
| 100% code coverage (llvm-cov ratchet) | N/A — Python lambda, no llvm-cov surface; covered by the 12 new/updated unit tests (FILE 3: 10 new + 1 updated back; FILE 4: 1 new front — the earlier "9" predated fixer rounds 1–3) + 2 workflow gates. CI-enforced at merge time since fixer round 4 via the ci.yml Repo Guards step "QuestDB console lambda unit tests (back + front)" (FILE 8) — before that step, NO CI lane ran these suites, so every "pinned by test_*" claim in this plan was a local-only check, not a merge-time ratchet |
| DHAT / Criterion / hot-path | N/A — cold-path observability lambda, zero Rust, zero hot-path code |
| Audit table + DEDUP keys | N/A — no SEBI-relevant event, no DB write; forensic record = Actions log + step summary + the front's structured `_log` |
| 100% logging | front `_log` structured JSON per request (unchanged, now shows ok/200 on "/") |
| 100% alerting | the FILE 5 deploy gate IS the alert — a red workflow on any shell regression; FILE 6 WARNs on the box lane. HONEST ENVELOPE (fixer round 3, 2026-07-06): the gate step's own continued EXISTENCE is pinned by NO ratchet — no source-scan guard test covers terraform-apply.yml gate content (the repo's pattern for that is a Rust guard test à la `crates/storage/tests/groww_scale_aws_lockout_guard.rs`, which is outside this PR's no-Rust scope constraint), so a future PR deleting or `continue-on-error`-ing the gate step leaves every Python and Rust suite green; the SAME envelope applies to the FILE 8 ci.yml test step (its continued existence is likewise pinned by no guard test). Flagged follow-up: add a workflow-content guard test pinning both steps (name + no continue-on-error) in a Rust-allowed PR |
| 100% security | secret hygiene: `::add-mask::` + header-FILE (umask 077 mktemp) + trap rm; device-key auth byte-untouched; SQL gates byte-untouched |
| 100% scenarios | the behavior matrix in the PR body (GET /, HEAD /, assets, /exec, box stopped, box slow, future unframed 3xx/non-3xx) — each row evidence-cited |
| 100% code review | adversarial pass on the diff before the PR opens |
| Zero ticks lost / WS / QuestDB resilience rows | N/A impact — nothing in the tick path, WS path, or QuestDB write path is touched; the console is a read-only observability surface |

Archive this plan to `.claude/plans/archive/2026-07-06-console-shell-hang-fix.md`
after push per `plan-enforcement.md`.

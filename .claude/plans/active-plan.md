# Implementation Plan: QuestDB console shell-hang fix (B4 r3) + deploy-gate smoke

**Status:** VERIFIED
**Date:** 2026-07-06
**Approved by:** Parthiban (operator directive 2026-07-06 — console shell-hang fix scope grant)
**Branch:** `claude/festive-bell-hm8w4d`
**Changed crates:** NONE (zero Rust — deploy/aws/lambda Python + workflows + one terraform comment only)
**Evidence base:** `deploy/aws/lambda/questdb-console-proxy/repro-evidence.md` (QuestDB 9.3.5 framing repro, 2026-07-06 — §9a/§9c/§10 cited throughout; COMMITTED verbatim in fixer round 7 2026-07-06 — the earlier session-scratchpad path was a dangling reference for readers of HEAD, charter §4 evidence discipline; fixer round 8 2026-07-07: the §5 empty xxd block was filled with a genuine `od -A x -t x1z` dump of the surviving original `/tmp/body2.out` capture file, per the file's round-8 provenance amendment; fixer round 9 2026-07-07: the §1–§8 curl COMMAND lines were re-quoted to reproducible form — the frozen lines were quote-stripped and could not have produced their own shown outputs (executed proof in the file's round-9 provenance amendment); every OUTPUT block byte-untouched; fixer round 10 2026-07-07: the round-9 amendment's claim that §9/§10 are "reproducible as written" was itself corrected — the two probe scripts those sections invoke (repro_backlambda.py, raw_socket_probe.py) were session-ephemeral and are NOT committed, so §9/§10 are RECONSTRUCTABLE (from §10's printed request bytes and handler.py's relay) rather than re-runnable from HEAD; §11's conclusions remain sustained by the frozen OUTPUT blocks themselves — see the file's round-10 provenance amendment) + `deploy/aws/lambda/questdb-console-proxy/gate-matrix-r7.sh` (the FILE 5 gate's behavioral scenario harness — COMMITTED in fixer round 8 2026-07-07, self-extracting from terraform-apply.yml so it is re-runnable from HEAD; the round-7 "scratchpad gate-matrix-r7.sh" citation was the same dangling-evidence class the repro-evidence.md commit fixed). Both files are EXCLUDED from the back-lambda zip (questdb-console.tf archive_file excludes, round 8)

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
- [x] FILE 5 — HARD deploy gate in terraform-apply.yml: new step "Deploy gate: console shell GET / must return 200 within 5s" inserted BEFORE "Publish QuestDB console URL to Telegram" (gate first, announce after). 3 attempts each hard-capped at 5s; body must contain the console shell HTML; 000/504/503 forgiven ONLY for a verifiably not-running box (terraform `instance_id` output → ec2 describe-instances; fixer round 1 2026-07-06: a STOPPED box manifests as CODE=000 through this gate — its ENI drops packets with no RST, so the back lambda's connect blocks the full 12s `_TIMEOUT_SECS` → upstream_timeout → front 504 at ~12s while the gate curl caps at 5s; a fast 503 needs a REFUSAL = box running with QuestDB down); unknown state FAILS CLOSED; a transient `terraform output questdb_console_url` failure also FAILS CLOSED (only the null/not-found console-disabled shape skips); fixer round 5 2026-07-06: running+503 gets ONE 90s grace re-probe before FATAL (a fast 503 on a RUNNING box is also the transient signature of QuestDB not yet listening — the 08:30 IST box auto-start window and the 15:46 IST `docker compose up -d` container-recreate window — and a grace pass still requires a genuine 200 + shell HTML, never a skip); fixer round 6 2026-07-06 (TOCTOU vs the 16:30 IST auto-stop cron): "verifiably not running" = not running at BOTH a pre-probe AND a post-probe EC2 state sample — a running→stopped transition across the probe window FAILS CLOSED (the failed probes were made against a running box; possible genuine regression must not become the auto-stop skip), a pre-sample `unknown` (transient describe-instances failure) still skips LOUDLY with both samples printed, and the 90s grace arm re-samples state AFTER its sleep so a mid-grace auto-stop re-enters the loud not-running skip instead of a stale "box IS running" false-FATAL (the terminal FATAL now prints the re-verified final-sample state); fixer round 7 2026-07-06 (start-side boot window): the 90s grace re-probe now covers running+000/504 as well as running+503 — the round-5 claim that "000/504 stay immediately FATAL because no start-up window produces them on a running box whose ENI is answering" was factually incomplete: EC2 flips to `running` within seconds of a start while the guest OS/network stack takes ~30-90s to come up, during which the ENI DROPS SYNs (no RST) → back lambda connect blocks 12s → front 504 → the 5s-capped gate curl reads 000 with state=running (BOTH samples can say running; `pending` lasts only seconds — so the start-side TOCTOU twin, pre-sample pending/stopped → post-sample running, is covered by the same unconditional grace). Fail-closed preserved: a grace pass still requires genuine 200 + shell HTML; a real shell-hang regression persists past the grace and FATALs ~90s later. HONEST RESIDUAL: an OS boot slower than the ~111s probe+grace budget still false-FATALs (bounded, disclosed). The mid-grace-stop SKIP wording no longer overclaims "proved QuestDB-not-listening" — a pre-grace fast 503 is also the front's mapping for a back-lambda crash (back_function_error/back_invoke_failed → generic 503), indistinguishable once the box stopped; the terminal FATAL and skip messages print the pre-grace code. Fixer round 8 2026-07-07: (a) the not-running SKIP message no longer labels a `pending` box as the "auto-stop window" — `pending` is the START transition (box still BOOTING: the 08:30 IST auto-start or an operator manual start), so the echo now names the window per state (skip DECISION unchanged; message accuracy only, the same wrong-window/cause message class rounds 5–7 fixed); (b) the behavioral-proof harness is COMMITTED at `deploy/aws/lambda/questdb-console-proxy/gate-matrix-r7.sh` — the earlier "scratchpad gate-matrix-r7.sh" citation was an untracked session-ephemeral path, unverifiable by any reader of HEAD (the exact charter-§4 dangling-evidence class round 7's repro-evidence.md commit fixed); the committed harness self-extracts the gate step from terraform-apply.yml (yaml.safe_load, `${{ env.* }}` substituted, fails on any surviving expression) so it re-runs from HEAD, counts failures, and grew to 13 scenarios (new L: pending-both-samples must name the start transition; E now pins the retained auto-stop wording). Behavioral proof: all 13 scenarios green on the round-8 workflow bytes (`bash deploy/aws/lambda/questdb-console-proxy/gate-matrix-r7.sh` → "gate-matrix: all 13 scenarios green"; verbatim output in the PR body). Secret hygiene: SSM read + ::add-mask:: + header-FILE (umask 077 mktemp), token never in argv. Fixer round 9 (2026-07-07): the harness is now CI-WIRED — the ci.yml Repo Guards step "QuestDB console lambda unit tests (back + front)" (FILE 8) runs `bash deploy/aws/lambda/questdb-console-proxy/gate-matrix-r7.sh` on every PR (the job feeds All Green; PyYAML preinstalled on ubuntu-latest; ~1-2s). In rounds 7-8 the harness ran in NO CI lane, so every "pinned by gate-matrix-r7.sh" comment (incl. the round-8 terraform-apply.yml skip-wording note) was a manual-run-only claim — the same unenforced-pin class round 4's ci.yml step fixed for the unittest suites. The wiring also closes half of the gate-existence ratchet gap (guarantee-matrix alerting row): the harness fails loudly ("gate step not found in terraform-apply.yml") if the gate step is deleted or renamed. Honest residual: a `continue-on-error:` added to the gate step is NOT caught (the harness executes the step's script, not its YAML attributes). Fixer round 10 (2026-07-07): (a) the round-9 plan edit had accidentally DELETED this item's mandatory file-manifest line (plan-enforcement.md rule 2), silently un-listing terraform-apply.yml + gate-matrix-r7.sh from the plan manifest and making plan-verify.sh's file check vacuous for exactly the two gate-critical files — restored below; (b) the mid-grace-auto-stop SKIP's cause enumeration is now PER pre-grace code (the old single message listed the 503-specific causes for ANY of 000/503/504, naming an inapplicable back-lambda-invoke-failure cause for 000/504 and omitting the hang class the terminal FATAL itself assigns to those codes — skip DECISION unchanged, message accuracy only, the same triage-misdirection class rounds 5-9 fixed); pinned by NEW harness scenario M (the 000 variant — scenario D pins only the 503 variant), growing the harness to 14 scenarios. Fixer round 11 (2026-07-08): (a) the not-running skip is 000-ONLY — a fast 503/504 with the box not running at BOTH samples now FAILS CLOSED as a console-lambda-layer fault (by the gate's own round-1 physics that code is impossible from a not-running box: its ENI blackholes SYNs → 12s back-lambda timeout → the 504 lands past the 5s curl cap = 000; the front maps back_not_configured/back_invoke_failed/back_function_error to a fast 503 — exactly the regression classes a terraform apply itself ships, previously laundered to exit-0 on every off-window apply, the gate's dominant regime; honest cost: a transient lambda-invoke throttle 503 now red-fails an off-hours apply — narrower than the false-OK); (b) harness grew to 18 scenarios — N pins the initial-probe 200-wrong-body FATAL and O the STATE_PRE=unknown loud-skip (both proven zero-coverage by surviving mutations in round 11), P/Q pin the new stopped+fast-503/504 FATAL; (c) the harness ABORTS on gate-step extraction failure instead of printing 14 misleading FAIL rows against an empty script (exit was already non-zero; triage honesty). Fixer round 12 (2026-07-08): the round-11 not-running fast-503/504 FATAL was NARROWED to where its lambda-layer physics is SOUND — a fast 504 with any not-running state (a 504 inside the 5s cap cannot be the back lambda's 12s upstream_timeout), or a fast 503 with BOTH samples stopped/pending. Two round-11 over-reaches fixed: (1) EC2 `stopping` — the guest OS stays alive mid-shutdown (docker stops QuestDB first, the still-alive kernel RSTs :9000 → back box_unreachable → the front's REAL fast 503 inside the 5s cap: the routine 16:30 IST auto-stop window; executed proof: codes `503 503 503` + states `stopping stopping` through the extracted gate hit the round-11 FATAL verbatim — a false-FATAL with a factually wrong "ENI drops SYNs" claim and a behavior regression vs the pre-round-11 loud skip) — stopping+503 now SKIPS LOUDLY with the same honest ambiguity as the mid-grace-stop arm; (2) STATE_PRE=unknown — the round-11 guard was merely `STATE_PRE != running` (strictly broader than the "not running at BOTH samples" premise its comment and message asserted), so it fired a self-contradictory "at both samples (pre-probe sample: unknown)" FATAL with an unsound "unambiguous lambda-layer" diagnosis (the box may have been RUNNING during the probes — a QuestDB-down connection-refusal 503 is a live alternative cause) — that case stays FAIL-CLOSED but with an honest AMBIGUOUS message; the top-of-step comment, the dispatch comment, and the round-1 "a fast 503 needs ... box RUNNING" note were corrected in lockstep. Harness grew to 21 scenarios (R: stopping+503 → loud skip; S: unknown-pre+503 → ambiguous FATAL; T: stopping+504 → still FATAL, pinning the narrowing boundary); non-vacuity: R and S FAIL when the new harness runs against the round-11 workflow bytes (reproducing the false-FATAL and the self-contradictory message verbatim) while T passes on both
  - Files: .github/workflows/terraform-apply.yml, deploy/aws/lambda/questdb-console-proxy/gate-matrix-r7.sh
  - Tests: N/A-for-crates-grep (the workflow gate itself IS the test; YAML validated via python3 yaml.safe_load; pre-fix bytes physically cannot pass — GET / needs ~12s while the per-attempt cap is 5s)
- [x] FILE 6 — box-local canary in deploy-aws.yml: two SSM command-array elements after the QUESTDB-UP loop, before the TRADING APP FIRST block — NON-FATAL loud WARN probing the rewrite TARGET `/index.html` (never bare `/`, which would hang the SSM shell) so a QuestDB image bump that breaks the shell is surfaced LOUDLY on the deploy that ships it — fixer round 1 (2026-07-06): the Fetch step now captures the SSM stdout to a file and greps for the canary WARN, emitting a job-level `::warning::` annotation + a `$GITHUB_STEP_SUMMARY` line (and a separate UNKNOWN warning when neither the WARN nor `QDB-SHELL-CANARY-OK` marker is present, e.g. SSM 24,000-char truncation). Non-fatal is mandated by deploy-aws.yml's own TRADING-APP-FIRST charter (a broken observability component must never block the trading deploy). Fixer round 11 (2026-07-08): the canary checks the HTTP code EXPLICITLY (`-w "%{http_code}"`, OK only on 200) — the old `curl -f` fails only on codes >= 400, so a FRAMED 3xx at /index.html (the rename-via-redirect drift shape most typical of a path relocation) exited 0 and echoed QDB-SHELL-CANARY-OK with zero WARN/annotation on the exact lane that ships QuestDB image bumps (executed proof, round-11 session: a local 301+Content-Length:0 server at /index.html → old command "QDB-SHELL-CANARY-OK"/exit 0, new command WARN "(got 301)"); the WARN line keeps the Fetch step's grep prefix. Fixer round 12 (2026-07-08): OK additionally requires curl EXIT 0 — the round-11 status-only compare (`$(curl ... || true)` then `[ code = 200 ]`) false-OKed an UNFRAMED keep-alive 200 at /index.html, the EXACT hang class this PR fixes: curl records %{http_code} from the status line even when the transfer subsequently fails, so headers-arrive-body-never-frames blocked to --max-time 5, exited 28, still printed 200, and `|| true` discarded the exit (executed proof, round-12 session: local unframed-200 server → `curl: (28) Operation timed out after 5002 milliseconds with 15 bytes received`, captured code 200, round-11 verdict QDB-SHELL-CANARY-OK, while the pre-round-11 `curl -f` on the same server correctly took the WARN arm — round 11 had regressed this class); the check is now `rc==0 AND code==200` and the WARN carries both the observed code and the curl exit (`got code=... curl_rc=...`)
  - Files: .github/workflows/deploy-aws.yml
  - Tests: N/A-for-crates-grep (echoes QDB-SHELL-CANARY-OK or a WARN line in the SSM output the workflow already surfaces; YAML validated)
- [x] FILE 7 — terraform timeout-comment truth-correction + archive_file excludes (round 8; fixer round 9 head reword — the item opened with "comment truth-correction ONLY" / "Zero infra diff" AFTER round 8 had already shipped a FUNCTIONAL `excludes` change in the same item: `excludes` is real archive_file configuration that determines the deployed zip contents and `output_base64sha256`, so both leading claims were false as stated and a heading-skimmer could conclude the tf change was droppable comment noise) (the stale "25s" claim → the back handler's real 12s `_TIMEOUT_SECS`; fixer round 4 also relabels "~16 sequential recvs" → "~16 sequential capped read() calls, each of which may span MANY recvs" — the ~16 counts `fp.read()` CALLS in `_read_capped`, ceil(4,100,000 / 262,144), not socket recvs; http.client's BufferedReader loops raw recvs per read() and each recv resets the 12s timer, so under a dribble the recv count is bounded only by the Lambda 26s kill). The COMMENT half is zero-infra; the round-8 excludes half IS an infra-relevant packaging change (below). NOT deploy-load-bearing (fixer round 4 correction — the earlier "LOAD-BEARING … makes the r3 lambdas deploy on the merge push" claim was wrong on BOTH halves, Verified: (1) this PR edits `.github/workflows/terraform-apply.yml` itself, which is IN the workflow's own push path filter (`paths: - 'deploy/aws/terraform/**' - '.github/workflows/terraform-apply.yml'`), so the merge push triggers the workflow with or without this tf touch; (2) the apply fires on `terraform plan -detailed-exitcode` = 2, which the changed lambda handler bytes already guarantee via `data.archive_file.questdb_console_proxy[0].output_base64sha256` → `source_code_hash` drift — no .tf edit is needed to produce drift. Nuance: the push-triggered apply is still market-hours-guarded, so a mid-market merge deploys via the 15:50 IST scheduled slot, not literally "on the merge push". Do NOT copy a "touch a tf comment to force deploy" pattern from this item — it is a non-mechanism). Fixer round 8 (2026-07-07, adversarially confirmed): the item is no longer comment-only — the round-7 commit of repro-evidence.md landed INSIDE the proxy `archive_file` source_dir while its `excludes` listed only `test_handler.py` + `README.md` (fixer round 10 truth-correction, 2026-07-07: the round-8 parenthetical here — "archive_file excludes are EXACT relative paths, no globs" — was FALSE for the provider CI resolves: versions.tf pins only hashicorp/aws and no .terraform.lock.hcl is tracked, so `terraform init` pulls the LATEST hashicorp/archive provider, whose docs state excludes "Supports glob file matching patterns including doublestar/globstar (**) patterns"; the committed entries are LITERAL filenames that still match under glob semantics — zero functional break — but the hand-listing policy stands on explicitness, not on a no-globs limitation, and a glob like `*.md`/`*.sh` is a legal alternative; same correction applied at the questdb-console.tf comment and gate-matrix-r7.sh header, the two sibling sites that replicated the claim), so the evidence markdown — and round 8's gate-matrix-r7.sh — would ship inside the prod back-lambda zip, violating the "ship handler.py only" packaging contract and making any future docs/harness edit drift `output_base64sha256` into a lambda redeploy from a non-code change (the same non-mechanism-deploy-trigger class this item warns about). Round 8 adds both filenames to `excludes` with an explaining comment. Fixer round 11 (2026-07-08): `"**/__pycache__/**"` added to BOTH console archive_file excludes — the "ship handler.py ONLY" contract covered only COMMITTED files, but this plan's own Test Plan command (`python3 -m unittest test_handler -v` run inside the source_dir) GENERATES gitignored `__pycache__/*.pyc` there (verified by execution; invisible to `git status`), so any NON-CI apply from a tree where the tests ever ran shipped bytecode in the prod zip AND drifted `output_base64sha256` (the CI apply path is a fresh checkout, unaffected); bytecode names are interpreter-version-dependent, so only a glob can pin them (legal per the round-10 provider-docs correction; executed proof: bmatcuk/doublestar — the provider's documented glob library — matches `__pycache__/handler.cpython-312.pyc` and `sub/__pycache__/x.pyc` against the pattern, and does NOT match handler.py or gate-matrix-r7.sh). HONEST ENVELOPE: a stale pre-glob archive provider cached for a local apply would not match the glob — no worse than the status quo
  - Files: deploy/aws/terraform/questdb-console.tf
  - Tests: N/A-for-crates-grep (terraform fmt: EXPECTED clean but NOT RUN — no terraform binary in the fix sandbox; charter §4: stated plainly rather than asserted; a miss fails loudly pre-apply via the workflow's own fmt-check-recursive step)
- [x] FILE 8 — CI merge-time enforcement of the lambda test suites (fixer round 4, 2026-07-06): new step "QuestDB console lambda unit tests (back + front)" appended to the EXISTING ci.yml `repo-guards` job (already in All Green's `needs:` — a step in an existing needed job, never a new job, per merge-gate-lock §5). Runs `python3 -m unittest test_handler -v` in both console lambda dirs unconditionally (stdlib-only suites, <1s). Closes the adversarially-confirmed gap that NO CI lane executed these suites — a PR partially reverting the r3 handler fix previously merged with every check green, caught only by the FILE 5 runtime gate on the next terraform-drift apply. Fixer round 9 (2026-07-07): the SAME step now also runs the FILE 5 gate's behavioral matrix (`gate-matrix-r7.sh` — 13 scenarios at round 9; 14 since round 10's scenario M; 18 since round 11's N/O/P/Q; 21 since round 12's R/S/T) — the round-4 fix covered the unittest suites but NOT the harness, leaving the round-8 "Pinned by gate-matrix-r7.sh scenarios E + L" workflow comment an unenforced claim (adversarially confirmed round 9); a skip-wording regression or a deleted/renamed gate step now goes red before merge (round 11: the deleted/renamed case is now a single-message harness ABORT instead of 14 misleading FAIL rows — exit was already non-zero)
  - Files: .github/workflows/ci.yml
  - Tests: N/A-for-crates-grep (the CI step itself executes the FILE 3 + FILE 4 suites at merge time; YAML validated via python3 yaml.safe_load)

## Design

**Root cause (Verified — deploy/aws/lambda/questdb-console-proxy/repro-evidence.md):** QuestDB 9.3.5 answers
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
  per-release hashed asset refs so the ASSET-HASH refs never drift on a
  QuestDB upgrade — the `/index.html` LOCATION itself IS a drift surface
  (see the "Future QuestDB renames /index.html" edge case: guarded by the
  FILE 6 box canary + the FILE 5 hard gate).
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
"Publish QuestDB console URL to Telegram" step. Honest scope (fixer round 12,
2026-07-08 — the earlier unqualified "a broken console is never announced as
live" was an absolute claim this plan's own disclosed exit-0 SKIP windows
contradict): on a gate PASS the console is verified before the announce; on
ANY exit-0 SKIP (console-disabled, verifiably-not-running 000, stopping-503,
mid-grace auto-stop, unknown-pre 000) the publish step still runs — it has no
dependency on PASS vs SKIP; a shell `exit 0` is indistinguishable to the
workflow — so the URL is announced UNVERIFIED, with the skip reason present
only in the run log/step summary. A broken console CAN therefore still be
announced inside those disclosed skip windows (e.g. a hang-class regression
spanning the grace sleep into the auto-stop, or a front/back hang-class
regression on a stopped box, where a healthy 12s-504 and the regression both
read as 000 through the 5s cap). What the gate guarantees is that a PROBEABLE
broken console is never announced. Each of its 3 attempts is hard-capped at
5s (`--max-time 5`), so the pre-fix behavior (504 after ~12s) can physically
never pass.

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
  via the terraform `instance_id` output; 000 with the box verifiably
  stopped/stopping/pending SKIPS LOUDLY (a not-running box is the console
  working correctly; round 8 — the skip message names the window per state:
  stopped/stopping = the auto-stop window, `pending` = the START transition
  with the box still booting — a stopped box shows as 000 through the 5s-capped curl,
  never a fast 503/504, because its ENI drops packets and the back lambda's
  connect blocks the full 12s socket timeout). Fixer round 11 (2026-07-08),
  NARROWED fixer round 12 (2026-07-08): the not-running skip is 000-ONLY,
  and a FAST 503/504 there FAILS CLOSED as a console-LAMBDA-layer fault
  (front/back wiring, lambda permission, back-lambda crash — the front maps
  back_not_configured/back_invoke_failed/back_function_error to a fast 503;
  pinned by gate-matrix scenarios P + Q) only where that diagnosis is SOUND:
  a fast 504 with any not-running state (a 504 inside the 5s cap cannot be
  the back lambda's 12s upstream_timeout — pinned by scenario T even for
  `stopping`), or a fast 503 with BOTH samples stopped/pending (no live
  kernel behind the ENI, so no RST is possible). Round 11's blanket claim
  ("unambiguously a lambda-layer fault" for ANY fast code on ANY not-running
  input) was physically wrong for two inputs the arm covered, both fixed in
  round 12: (1) EC2 `stopping` — the guest OS stays alive for tens of
  seconds mid-shutdown, docker stops QuestDB first, and the still-alive
  kernel RSTs :9000 → back box_unreachable → the front's REAL fast 503
  inside the 5s cap (the routine 16:30 IST auto-stop window the gate's own
  round-6 comment documents; executed proof: codes `503 503 503` + states
  `stopping stopping` hit the round-11 FATAL verbatim) — round 12 routes
  stopping+503 through a LOUD SKIP with the same honest ambiguity as the
  mid-grace-stop arm (benign stop transition OR a back-lambda fault,
  unresolvable once the box is down; pinned by scenario R); (2)
  STATE_PRE=unknown — the code's guard was merely `STATE_PRE != running`,
  strictly broader than the comment's "not running at BOTH samples" premise,
  so the FATAL fired with a self-contradictory "at both samples (pre-probe
  sample: unknown)" message and an unsound diagnosis (the box may have been
  RUNNING during the probes, making a QuestDB-down connection-refusal 503 a
  live alternative cause) — round 12 keeps that case FAIL-CLOSED but with an
  honest AMBIGUOUS message (lambda-layer OR a then-running box with QuestDB
  down that stopped before the post-sample — unresolvable post-stop; pinned
  by scenario S). HONEST COST (disclosed): a transient lambda-invoke
  throttle producing a fast 503 with the box stopped at both samples still
  red-fails an off-hours apply — strictly narrower than the false-OK it
  replaces. Any of 000/503/504 with the box RUNNING fails closed only AFTER
  the running arm's ONE 90s grace re-probe (round 7 — the grace covers
  000/504 as well as 503, absorbing the post-start OS-boot window where
  state=running but the ENI drops SYNs); an UNKNOWN post-probe state FAILS
  CLOSED immediately (audit Rule 11: no false-OK). Fixer round 6 (2026-07-06, TOCTOU): the state is sampled BEFORE
  the probes as well as after — a running→stopped/stopping transition across
  the probe window (an apply landing at ~16:29 IST, auto-stop firing after
  the failed probes but before the state read) FAILS CLOSED instead of
  skipping, because those probes were made against a RUNNING box; and the
  90s grace arm re-samples after its sleep, so a box that auto-stops
  mid-grace re-enters the loud not-running skip instead of a false-FATAL
  asserting "the box IS running" from the pre-sleep sample.
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
  fault never blocks the trading deploy). Fixer round 11 (2026-07-08): the
  canary requires HTTP 200 EXPLICITLY — its old `curl -f` failed only on
  codes >= 400, so a FRAMED 3xx at /index.html (the rename-via-redirect
  drift shape most typical of a path relocation) exited 0 and echoed
  QDB-SHELL-CANARY-OK with no WARN/annotation. Fixer round 12 (2026-07-08):
  the round-11 status-only compare was itself blind to an UNFRAMED
  keep-alive 200 at /index.html — the exact hang class this PR fixes: curl
  records `%{http_code}` from the STATUS LINE even when the transfer then
  FAILS, so headers-arrive-body-never-frames blocked to `--max-time 5`,
  exited 28, still printed 200, and the `|| true` discarded the exit — the
  round-11 canary echoed QDB-SHELL-CANARY-OK (executed proof, round-12
  session: local unframed-200 server → `curl: (28) Operation timed out
  after 5002 milliseconds with 15 bytes received`, captured code `200`,
  verdict CANARY-OK; the pre-round-11 `curl -f` correctly took the WARN arm
  on the same server — so round 11 was a regression for this class). The canary
  now WARNs unless curl exits 0 AND the code is 200, carrying both the
  observed code and the curl exit in the WARN line.

## Failure Modes

- **Genuinely slow box** — still an honest 504 (`upstream_timeout` mapping
  UNCHANGED — back handler timeout arms + the front's 504 mapping) — and
  post-fix that 504 finally MEANS slow, not "unframed 301".
- **Dead box** — no static shell exists that could fake a 200 on a dead box.
  Precise codes (fixer round 1, 2026-07-06): a STOPPED box drops packets (no
  RST), so the console answers 504 `upstream_timeout` after the back lambda's
  12s connect timeout — the front's 503 `box_unreachable` needs a connection
  REFUSAL (box running, QuestDB down). The smoke gate independently verifies
  the EC2 state — at BOTH a pre-probe and a post-probe sample since fixer
  round 6 (2026-07-06), so an auto-stop landing mid-window cannot launder a
  running-box failure into the skip — and since fixer round 11 (2026-07-08)
  accepts ONLY 000 as the not-running window. Fixer round 12 (2026-07-08)
  scopes the round-11 lambda-layer FATAL to where its physics actually
  holds: a fast 504 (any not-running state — a 504 inside the 5s cap cannot
  be the back lambda's 12s upstream_timeout), or a fast 503 with BOTH
  samples stopped/pending (only a box with NO live kernel behind its ENI
  physically cannot RST). A fast 503 with a `stopping` sample SKIPS loudly
  instead — during EC2 `stopping` the still-alive shutdown kernel DOES RST
  :9000 after docker stops QuestDB, minting a real fast 503 (the round-11
  arm false-FATALed that routine auto-stop window with a factually wrong
  impossibility claim) — and a fast 503/504 with STATE_PRE=unknown fails
  closed with an honest AMBIGUOUS message (the "both samples" premise is
  unverified; the box may have been running during the probes).
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
- **FILE 5 gate behavioral matrix (round 8; CI-wired round 9; 21 scenarios
  since round 12):**
  `bash deploy/aws/lambda/questdb-console-proxy/gate-matrix-r7.sh` — the
  committed, self-extracting harness re-runs from HEAD against
  the REAL gate step (stubbed terraform/aws/curl/sleep); round-8 run (then
  13 scenarios): "gate-matrix: all 13 scenarios green"; round-10 run:
  "gate-matrix: all 14 scenarios green"; round-11 run:
  "gate-matrix: all 18 scenarios green"; round-12 run:
  "gate-matrix: all 21 scenarios green" (verbatim per-scenario table in the
  PR body). Round-12 non-vacuity: the 21-scenario harness re-run against
  the ROUND-11 workflow bytes fails exactly R (the stopping+503 false-FATAL
  fires, reproducing the wrong "ENI drops SYNs" claim verbatim) and S (the
  self-contradictory "at both samples (pre-probe sample: unknown)" message)
  while T passes on both — "gate-matrix: 2 of 21 scenario(s) FAILED".
  Round-10 non-vacuity: scenario M re-run against the PRE-round-10
  workflow bytes FAILS ("FAIL(missing: a booting OS/ENI still dropping
  SYNs)") — also proving the round-10 verdict-label fix (rc matched, grep
  missed → the row prints a clean FAIL, not the old contradictory
  "PASS FAIL(...)"). Round-11 non-vacuity (each mutation killed by exactly
  its new scenario): deleting the initial-probe body check → only N fails;
  flipping the STATE_PRE=unknown skip into a FATAL → only O fails; removing
  the round-11 stopped+fast-503/504 FATAL → only P+Q fail. Round-11
  extraction-abort: renaming the gate step now yields the single "gate step
  not found in terraform-apply.yml" + ABORT line, exit 1 — no scenario rows
  (previously 14 misleading FAIL rows against an empty script; exit was
  already non-zero). The harness rm -rf's its mktemp work tree via an
  EXIT trap (round 10 — previously one leaked temp tree per run). Since
  fixer round 9 (2026-07-07) the harness ALSO runs on every
  PR inside the FILE 8 ci.yml Repo Guards step — before that it was
  manual-run-only (a disclosure this Test Plan previously omitted).
- Workflow YAML: `python3 -c "import yaml; yaml.safe_load(...)"` on all
  edited workflows (incl. ci.yml). `terraform -chdir=deploy/aws/terraform
  fmt -check`: EXPECTED clean but NOT RUN — no terraform binary in the fix
  sandbox (charter §4: stated plainly, not asserted); the workflow's own
  `terraform fmt -check -recursive` step fails loudly pre-apply on any miss.

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
| 100% alerting | the FILE 5 deploy gate IS the alert — a red workflow on any shell regression; FILE 6 WARNs on the box lane (round 11, 2026-07-08: the FILE 6 canary requires HTTP 200 EXPLICITLY — the old `curl -f` passed a FRAMED 3xx at /index.html, so the rename-via-redirect drift shape produced NO WARN on the very lane that ships QuestDB image bumps; executed proof in the round-11 review + fix session. Round 12, 2026-07-08: the canary ALSO requires curl exit 0 — the round-11 status-only compare false-OKed an UNFRAMED keep-alive 200 at /index.html, the exact hang class this PR fixes: curl prints the status-line 200 even when the transfer then times out at --max-time 5 with exit 28 and `|| true` discarded it; executed proof in the round-12 session — a regression vs the pre-round-11 `curl -f` for that class). HONEST ENVELOPE (fixer round 3, 2026-07-06; NARROWED fixer round 9, 2026-07-07; CORRECTED AGAIN fixer round 11, 2026-07-08): since round 9 the gate step's DELETION or RENAME fails CI at merge time — the FILE 8 ci.yml step runs `gate-matrix-r7.sh`, whose extractor failure now ABORTS the harness with the single loud "gate step not found in terraform-apply.yml" line (round 11 — previously the extractor error was followed by 14 misleading FAIL rows against an empty script; exit code was already non-zero, so the ratchet held). Its 21 scenarios (13 in round 9; M added round 10; N/O/P/Q added round 11; R/S/T added round 12 — the stopping-503 skip, the unknown-pre ambiguous FATAL, and the stopping-504 FATAL boundary) pin the gate script's PROBE/STATE/GRACE fail-closed arms + skip wording. ROUND-11 TRUTH-CORRECTION: the round-10 claim that EXACTLY three gate branches were uncovered ("a regression in any of those goes red nowhere" implying every other branch goes red) was itself an undercount — two MORE zero-coverage branches were proven by surviving mutations: the INITIAL-probe 200-wrong-body FATAL (deleting the check left all prior scenarios green; only the grace-arm twin was pinned) and the STATE_PRE=unknown loud-skip policy (flipping it into a FATAL left all prior scenarios green). Unlike the three stub-limited branches, both were plain scenario omissions expressible with the existing stubs — they are NOW pinned by scenarios N + O (mutation-verified in round 11: each mutation is killed by exactly its new scenario). The KNOWN-uncovered set is now the three stub-limited branches — the terraform-output fail-closed FATAL incl. its load-bearing disabled-vs-transient classification grep, the console-disabled loud-skip arm itself, and the SSM-failure `set -euo pipefail` abort (the harness's terraform stub always succeeds for questdb_console_url and its aws stub always returns a token for ssm) — a regression in any of THOSE goes red nowhere, and NO exhaustive-branch-coverage claim is made for the remainder (round 10's "exactly three" enumeration is precisely the overclaim round 11 disproved). In rounds 3-8 the harness ran in no CI lane, so this row's gap was wider than disclosed — the harness itself was manual-run-only. STILL un-ratcheted: (a) a `continue-on-error:` added to the gate step (the harness executes the step's `run` script, not its YAML attributes); (b) the FILE 8 ci.yml step's own continued existence (deleting THAT step silently un-wires both the unittest suites and the harness); (c) the three stub-limited gate branches just named (would need per-scenario failure-shape stubs the harness's fixed terraform/aws stubs do not support today — flagged follow-up alongside the workflow-content guard); (d) — added round 12, previously an UNDISCLOSED omission from this very enumeration — the ENTIRE FILE 6 canary layer in deploy-aws.yml: the two SSM canary command elements (incl. the round-11/12 `rc==0 AND code==200` semantics) and the Fetch step's grep/`::warning::` arms have ZERO ratchet — grep for `QDB-SHELL-CANARY`/`QDB_CANARY` across the repo hits only deploy-aws.yml and this plan, no unit test exists, and gate-matrix-r7.sh extracts exclusively from terraform-apply.yml, so a revert to `curl -f` (re-opening the executed-proof framed-3xx blindness), a revert to the round-11 status-only compare (re-opening the unframed-200 blindness), or outright deletion of the canary/grep arms merges with all unit tests + all 21 harness scenarios green — on the ONLY same-day drift guard of the lane that ships QuestDB image bumps (per this plan's own Edge Cases drift analysis). Flagged follow-up: a workflow-content guard test pinning both steps (name + no continue-on-error) in a Rust-allowed PR (the repo's pattern: `crates/storage/tests/groww_scale_aws_lockout_guard.rs`) — EXTENDED round 12 to also pin deploy-aws.yml's canary command elements + the Fetch-step grep arms |
| 100% security | secret hygiene: `::add-mask::` + header-FILE (umask 077 mktemp) + trap rm; device-key auth byte-untouched; SQL gates byte-untouched |
| 100% scenarios | the behavior matrix in the PR body (GET /, HEAD /, assets, /exec, box stopped, box slow, future unframed 3xx/non-3xx) — each row evidence-cited |
| 100% code review | adversarial pass on the diff before the PR opens |
| Zero ticks lost / WS / QuestDB resilience rows | N/A impact — nothing in the tick path, WS path, or QuestDB write path is touched; the console is a read-only observability surface |

Archive this plan to `.claude/plans/archive/2026-07-06-console-shell-hang-fix.md`
after push per `plan-enforcement.md`.

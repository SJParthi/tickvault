# Implementation Plan: B4 — QuestDB One-Click Console (device-key gated, read-only, SG stays closed)

**Status:** APPROVED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — task brief 2026-07-03 ("B4 · QuestDB one-click console — Branch claude/questdb-console-proxy … device-key auth; server-side read-only SQL gate … box SG stays closed. Terraform + tests + live browser proof.")

## Design

```
Browser ──HTTPS──▶ API-Gateway v2 HTTP API (tv-<env>-questdb-console, $default, payload v2)
                        │
                        ▼
              FRONT Lambda (NON-VPC — tv-<env>-questdb-console-front)
              · device-key auth: one-click HMAC link token (≤90s) minted by the
                operator portal, OR paste-the-key login page, OR Bearer header
              · session cookie qdb_sess (HttpOnly; Secure; SameSite=Lax; 12h),
                HMAC-signed over the SAME SSM control secret the portal uses
                (/tickvault/<env>/operator/control-secret — runtime SSM read,
                60s cache, fail-closed; NEVER in env vars / TF state)
              · server-side READ-ONLY SQL gate on /exec + /exp — verbatim
                mirror of operator-control _is_safe_sql + _cap_sql_rows
              · path/method whitelist (GET/HEAD only; deny-by-default)
                        │ lambda:InvokeFunction (JSON relay envelope)
                        ▼
              BACK Lambda (VPC-ATTACHED — tv-<env>-questdb-console-proxy)
              · dumb HTTP relay to http://<box_private_ip>:9000 (TF-injected
                from aws_instance.tv_app.private_ip — never hardcoded)
              · ZERO secrets, ZERO AWS SDK calls at runtime
              · defense-in-depth: re-runs the SAME SQL gate + path whitelist
              · 5.5 MB response cap, 25s timeout
                        │ TCP 9000 (SG-to-SG only)
                        ▼
              QuestDB web console on the box (port 9000 NEVER public)
```

WHY TWO LAMBDAS: a VPC Lambda in this VPC has **no internet/AWS-API path**
(single public subnet, no NAT, no VPC endpoints) so it cannot read the SSM
device-key secret at runtime. Splitting front (non-VPC: secret handling
identical to the existing operator-control posture — runtime SSM read, 60s
cache, fail-closed) from back (VPC: secret-free relay) keeps the secret out of
the VPC hop entirely and keeps the box SG closed except ONE new SG-to-SG
ingress rule: TCP 9000 from the back-Lambda SG only.

SG rule mechanism: `aws_security_group.tv_app` uses **inline** ingress blocks
(main.tf). Mixing an external `aws_security_group_rule` /
`aws_vpc_security_group_ingress_rule` with inline blocks makes the inline SG
fight/drift against the external rule on every plan. Therefore the 9000 rule
is a `dynamic "ingress"` block INSIDE the tv_app SG in main.tf, keyed on
`var.enable_questdb_console` — zero drift, provider-safe with aws ~>5.80.

Portal integration: operator-control gains action `qdb_console_url` (mints the
90s one-click HMAC link from `QDB_CONSOLE_URL` env injected by TF) + one
`🗄 Open QuestDB Console` button in the Data-tab DB console card.

## Edge Cases

- Box auto-stopped (outside 08:30–16:30 IST): back returns `box_unreachable` →
  front returns 503 with an honest "box is offline" message, never a hang.
- Expired/forged/garbage link token or cookie → 401 + login page (no stack
  trace, no secret echo). Token replay after 90s → 401.
- SQL smuggling: `;` chaining, `--` / `/* */` comments, banned mutators
  (insert/update/delete/drop/…/backup/checkpoint/snapshot/…) anywhere,
  `with` CTE hiding a mutator, over-length query (>20 000 chars), empty /
  whitespace-only query — all rejected 400 with a QuestDB-shaped error JSON
  so the console UI renders it.
- Non-GET/HEAD methods → 405 (blocks /imp uploads + settings writes by
  method); `/imp` and unknown paths → 403 even on GET.
- Response >5.5 MB from QuestDB → honest 502 (Lambda payload limit is 6 MB).
- SSM secret missing/unreadable → fail-closed deny-all (same as portal).
- QDB_CONSOLE_URL env empty (console disabled) → portal action returns
  `{"error":"console not enabled"}`; button shows the error toast.
- `limit` param on /exec: numeric forms clamped to ≤1000; SQL-level LIMIT is
  clamped/appended by the `_cap_sql_rows` mirror.

## Failure Modes

| Failure | Behaviour | Signal |
|---|---|---|
| SSM unreachable from front | deny-all 401 (fail-closed) | CloudWatch front-Lambda logs `denied_auth` |
| Back Lambda ENI/VPC failure | front 502/503 honest JSON | `tv-<env>-questdb-console-front-errors` alarm → SNS |
| Box stopped / 9000 down | back `{"err":"box_unreachable"}` → front 503 | structured log `outcome=back_error` |
| SQL gate bypass attempt | 400 QuestDB-shaped error; logged `denied_sql` with 200-char sql_head (never secrets) | structured JSON log |
| Oversized response | 502 `response too large` | structured log |
| Terraform flag off | zero resources created (count-gated), portal button returns "console not enabled" | n/a |

## Test Plan

Python stdlib `unittest` (mirrors `deploy/aws/lambda/operator-control/test_handler.py`),
run with `python3 -m unittest test_handler` in each Lambda dir:

- `deploy/aws/lambda/questdb-console-front/test_handler.py` — SQL-gate parity
  with operator-control (multi-statement, trailing `;`, comments, banned words
  any case/position, CTE smuggling, over-length, empty), auth (missing /
  expired / forged / garbage token+cookie, Bearer right/wrong, login POST
  right/wrong, `hmac.compare_digest` used — asserted via source inspection),
  path/method gate (/imp 403, POST /exec 405, /assets/app.js forwarded,
  unknown 403, /exec without query 400), response plumbing (base64 binary
  passthrough, content-encoding preserved, >5.5MB → 502, box_unreachable →
  503), `test_build_marker_present`.
- `deploy/aws/lambda/questdb-console-proxy/test_handler.py` — re-gate on
  /exec+/exp, path/method whitelist parity, size cap, timeout/URLError →
  box_unreachable, build marker.
- `deploy/aws/lambda/operator-control/test_handler.py` — new
  `QdbConsoleUrlAction` tests: enabled path returns signed `/open?tok=` URL
  with valid HMAC + ~90s expiry; disabled (empty env) returns error; HTML
  contains the console button.

## Rollback

- Flip `TF_VAR_enable_questdb_console` to `false` (or remove the env line in
  terraform-apply.yml) and apply: every resource is `count`-gated, so API-GW,
  both Lambdas, both IAM roles, the Lambda SG AND the tv_app 9000 ingress
  (dynamic block keyed on the same var) all destroy cleanly. Box SG returns to
  SSH-only. Portal button degrades to "console not enabled" (env empty).
- Pure `git revert` of the feature commits is equally safe — no data
  migration, no Rust code, no hot-path involvement.

## Observability

- Front Lambda: structured JSON log per request
  `{"evt":"qdb_console","path","method","outcome":"ok|denied_auth|denied_sql|denied_path|back_error","sql_head","status"}`
  → CloudWatch Logs (no secrets/tokens/cookies ever logged).
- CloudWatch alarm `tv-<env>-questdb-console-front-errors` on Lambda Errors
  (mirror of the operator-control alarm; treat_missing_data notBreaching).
- `output "questdb_console_url"` + terraform-apply publishes the URL to the
  private SNS→Telegram channel (mirrors the portal-link step).
- Deployed-bytes proof marker: `QDB_CONSOLE_BUILD = "b4-qdb-console-2026-07-03-r1"`
  in BOTH handlers, ratcheted by `test_build_marker_present`.

## Plan Items

- [x] Front Lambda handler (auth + SQL gate + path/method whitelist + relay)
  - Files: deploy/aws/lambda/questdb-console-front/handler.py
  - Tests: test_sql_gate_parity_with_operator_control, test_multi_statement_rejected, test_comment_smuggling_rejected, test_banned_words_rejected_any_case, test_forged_token_rejected, test_expired_cookie_rejected, test_bearer_right_key_authorized, test_login_post_sets_cookie, test_imp_path_403, test_post_exec_405, test_assets_forwarded, test_oversize_body_502, test_box_unreachable_503, test_build_marker_present
- [x] Back Lambda handler (VPC relay, zero secrets, re-gate)
  - Files: deploy/aws/lambda/questdb-console-proxy/handler.py
  - Tests: test_regate_rejects_mutator_sql, test_path_whitelist_parity, test_size_cap_returns_too_large, test_urlerror_maps_to_box_unreachable, test_no_boto3_import, test_build_marker_present
- [x] Terraform: SGs + dynamic 9000 ingress + both Lambdas + API-GW + alarm + output + var
  - Files: deploy/aws/terraform/questdb-console.tf, deploy/aws/terraform/main.tf, deploy/aws/terraform/variables.tf, deploy/aws/terraform/operator-control-lambda.tf
  - Tests: (terraform fmt; validate unavailable in container — honest note in report)
- [x] Portal one-click button + `qdb_console_url` action
  - Files: deploy/aws/lambda/operator-control/handler.py
  - Tests: test_qdb_console_url_disabled_returns_error, test_qdb_console_url_enabled_returns_signed_link, test_html_has_qdb_console_button
- [x] Tests for all three Lambda dirs (exhaustive gates)
  - Files: deploy/aws/lambda/questdb-console-front/test_handler.py, deploy/aws/lambda/questdb-console-proxy/test_handler.py, deploy/aws/lambda/operator-control/test_handler.py
  - Tests: (see items above)
- [x] Workflow wiring: TF_VAR + console-URL Telegram publish
  - Files: .github/workflows/terraform-apply.yml
  - Tests: (YAML change; exercised by the next terraform-apply run)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Operator clicks 🗄 button in portal | new tab opens console, auto-unlocked via 90s link token → 12h cookie |
| 2 | Operator opens console URL cold on a new browser | login page; pasting device key unlocks |
| 3 | `drop table ticks` typed into console | 400 QuestDB-shaped error rendered in the console grid |
| 4 | Console opened at 20:00 IST (box stopped) | 503 "box is offline (auto-stopped outside 08:30–16:30 IST)" |
| 5 | Attacker replays a Telegram link token after 90s | 401 login page |
| 6 | Port-scan of the box on 9000 | closed — SG allows only the back-Lambda SG |

## Per-Item Guarantee Matrix

Cross-reference: `.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row canonical form). Filled for THIS item:

| Demand | This item's proof |
|---|---|
| 100% code coverage | Pure-function handlers fully unit-tested (3 test suites, stdlib unittest). Rust coverage gate untouched (no Rust change). |
| 100% audit coverage | Structured JSON request log per console request (CloudWatch, 14d) — outcome-labelled; no QuestDB audit table (read-only path, no state mutated). N/A for `<event>_audit` — nothing SEBI-relevant is written. |
| 100% testing coverage | Unit (gate/auth/plumbing) + parity tests vs operator-control `_is_safe_sql`. Chaos/loom/DHAT N/A — Python cold-path Lambda, no Rust hot path. |
| 100% code checks | pre-commit gates (banned-pattern, secret-scan, fmt) run on every commit; terraform fmt applied. |
| 100% code performance | N/A — no Rust hot-path change; Lambda cold path only (≤29s bounded). |
| 100% monitoring | CloudWatch Lambda metrics + Errors alarm → SNS → Telegram. |
| 100% logging | Structured JSON per request; secrets/tokens NEVER logged (tested by inspection + no-echo design). |
| 100% alerting | `tv-<env>-questdb-console-front-errors` alarm (mirror of operator-control alarm). |
| 100% security | Device-key auth (constant-time), HMAC-signed short-TTL link + session cookie, fail-closed SSM read, read-only SQL gate mirrored twice (front + back), deny-by-default path/method whitelist, SG-to-SG only ingress, zero secrets in VPC Lambda / env / TF state. |
| 100% security hardening | HttpOnly/Secure/SameSite cookie; no external assets on login page; 5.5MB body cap; 20K SQL length cap; referrer/cache headers on login page. |
| 100% bugs fixing | Exhaustive adversarial test list (smuggling, replay, forging) written first as ratchets. |
| 100% scenarios covering | 6-row scenario table above incl. box-offline + port-scan. |
| 100% functionalities covering | Every new function exercised by a named test; portal action tested enabled + disabled. |
| 100% code review | Diff reviewed against operator-control patterns (parity asserted in tests). |
| 100% extreme check | `test_build_marker_present` + SQL-gate parity tests fail the build on regression. |

7-row resilience matrix (honest N/A where not applicable):

| Demand | This item |
|---|---|
| Zero ticks lost | N/A impact — read-only console; introduces NO tick path change. Ring→spill→DLQ untouched. |
| WS never disconnects | N/A — no WebSocket code touched. |
| Never slow/locked/hanged | Every network leg bounded: SSM 60s cache, urllib timeout 25s, front Lambda timeout 29s < API-GW 30s cap. |
| QuestDB never fails | Console is a read-only client; failure degrades to honest 502/503, never affects the app's ILP write path. |
| O(1) latency | N/A — no hot-path change. Honest flag: SQL-gate scan is O(len(query)) per request, bounded by MAX_SQL_LEN=20 000. |
| Uniqueness + dedup | N/A — no table writes. |
| Real-time proof | Errors alarm + structured logs + `QDB_CONSOLE_BUILD` deployed-bytes marker. |

**Honest 100% claim:** 100% inside the tested envelope, with ratcheted
regression coverage: single-statement SELECT/SHOW/EXPLAIN/WITH only (verbatim
`_is_safe_sql` mirror, parity-tested against operator-control); 1000-row cap;
20 000-char query cap; 5.5 MB response cap; 90s link-token + 12h session
envelope; box SG opens TCP 9000 to the back-Lambda SG ONLY (never public).
Beyond the envelope the gate fails CLOSED (401/403/405/400/502/503) — it never
falls open. This console cannot claim availability when the box is auto-stopped
(16:30–08:30 IST); it says so honestly with a 503.

# Dhan REST-Health Canary — Error Codes (REST-CANARY-01)

> **⚠ RETIRED 2026-07-14 (operator Dhan noise lock —
> `dhan-rest-only-noise-lock-2026-07-14.md` §0 verbatim quote: "for Dhan
> except spot 1m and option chain nothing else should work…").** The canary
> module `crates/app/src/rest_canary_boot.rs`, its two spawn sites
> (`dhan_rest_stack.rs` Phase 5b + the dead lane site in main.rs), and the
> `rest-canary-01` CloudWatch log-filter/alarm entry
> (`deploy/aws/terraform/error-code-alarms.tf`) are DELETED. Rationale: the
> retained spot-1m + option-chain legs self-detect a dead REST surface within
> ~3-4 minutes via their own persist-gated escalation edges
> (`Spot1mFetchDegraded` / `ChainFetchDegraded`) — strictly better coverage
> than 3 fixed daily probe slots. The `ErrorCode::RestCanary01ProbeFailed`
> variant is RETAINED until the C4 variant sweep (this file keeps satisfying
> the cross-ref test). Content below retained as historical audit.

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C > this file.
> **Operator directive (2026-06-10, task DHAN-REST-400):** *"one cheap GET
> /v2/profile at 09:05 + 12:00 + 15:25 IST; on non-200, page HIGH immediately
> with the captured body — never again discover at 15:33 that REST died at
> 08:45."*
> **Delivery boundary (2026-07-06):** until this date the "page HIGH
> immediately" demand was NOT met — the `error!` reached only the log sinks
> (the CloudWatch-only migration retired the Loki→Alertmanager→Telegram route
> with no replacement; the 2026-07-06 12:00 IST probe failure produced ZERO
> pages). Routing is now the metric-filter alarm chain: `error!` →
> errors.jsonl (`data/logs/machine/`) → CloudWatch Logs `/tickvault/prod/app`
> (CW agent) → `tv-<env>-errcode-rest-canary-01` log metric filter → metric →
> alarm (≤5 min) → SNS `tv-prod-alerts` → Telegram webhook Lambda
> (`deploy/aws/terraform/error-code-alarms.tf`).
> **Companion code:** `crates/app/src/rest_canary_boot.rs`,
> `crates/common/src/sanitize.rs::capture_rest_error_body`,
> `crates/common/src/url_join.rs::join_api_url`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires
> this file to mention every `RestCanary*` variant.

---

## §0. Why this exists (the 2026-06-10 incident)

On 2026-06-10 every `api.dhan.co` REST call returned HTTP 400 — profile + getIP
at 08:45, then ALL 776/776 post-market cross-verify intraday fetches at 15:33.
The WebSocket feed was unaffected (separate URL construction + connection), so
nothing paged between 08:45 and 15:33. Worse, the error paths logged only
`"http 400"` and DROPPED Dhan's response body — the `errorType` / `errorCode` /
`errorMessage` fields that name the cause (data-plan expiry vs malformed
request vs WAF block) were never captured, and the final request URL was never
logged (a trailing-slash base-URL override producing `…/v2//path` would 400
every endpoint at once — exactly this all-at-once signature).

The canary closes the detection gap; the body/URL capture closes the
root-cause-visibility gap; `join_api_url` closes the malformed-URL gap.

## §1. REST-CANARY-01 — scheduled REST-health probe failed

**Severity:** High. **Auto-triage:** No (operator must read the captured body).

**Trigger:** the canary task probes `GET /v2/profile` (with `access-token`
header) at **09:05, 12:00 and 15:25 IST** on trading days. A probe fails when:
- the response status is non-2xx (the dominant case — pages within ~1-5 min
  via the `tv-<env>-errcode-rest-canary-01` CloudWatch log-filter alarm, added
  2026-07-06), or
- the HTTP send leg fails twice (one retry after 30s — mirrors the mid-session
  watchdog's 2026-04-26 transient-network lesson so a laptop DNS blip does not
  page), or
- no access token is available at probe time (REST cannot be healthy without
  one).

**The `error!` payload carries:**
- `status` — the HTTP status (or `send_failed`)
- `url` — the EXACT final request URL, token-redacted (`redact_url_params`)
- `body` — ≤300-char secret-redacted response body
  (`capture_rest_error_body` — JWT/PIN/TOTP/clientId can never appear;
  ratcheted by `test_capture_rest_error_body_redacts_jwt`)

**Triage:**
1. Read the `body` field — Dhan's `errorCode`/`errorMessage` names the cause:
   - `DH-902` / "not subscribed" → Data-API plan expired — renew on Dhan portal.
   - `DH-901` / 401 → token invalid — check `tv_token_remaining_seconds`,
     rotate via restart.
   - HTML / WAF text → Dhan-side gateway issue — file support ticket with the
     captured body verbatim.
2. Read the `url` field — `//` anywhere after the scheme means a malformed
   base-URL override (trailing slash). `join_api_url` makes this impossible
   for code-built URLs; if it appears, an env override bypassed the helper —
   fix the override.
3. Cross-check the WS feed (`tv_websocket_connections_active`): WS healthy +
   REST failing = REST-surface-only issue (auth header, URL, data plan); both
   failing = network/token issue — see `AUTH-GAP-*` runbooks.
4. The 15-minute mid-session profile watchdog
   (`mid_session_watchdog.rs`) pages CRITICAL on profile invalidation
   independently; the canary's value is the captured body + URL + the fixed
   probe times that bracket the trading session.

**Metrics:** `tv_rest_canary_probes_total{outcome="pass"|"fail"}`.

**Source:**
- `crates/common/src/error_code.rs::ErrorCode::RestCanary01ProbeFailed`
- `crates/app/src/rest_canary_boot.rs` (schedule + classify + runner)
- Boot wiring: `crates/app/src/main.rs`

## §2. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `RestCanary*` variant)
- `crates/app/src/rest_canary_boot.rs`
- `crates/common/src/url_join.rs`
- Any file containing `REST-CANARY-` or `RestCanary0` or `capture_rest_error_body`

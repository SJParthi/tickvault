# Runbook — Auth / Token failures

**When it fires:** `DH-901`, `DATA-807`, `DATA-808`, `DATA-809`,
`DATA-810`, `AUTH-GAP-01`, `AUTH-GAP-02`, `AuthCircuitBreakerOpen`,
`TokenExpiryCritical`, `TokenRenewalFailureBurst`.

**Consequence if not resolved:** trading halts (auth circuit breaker
opens after N consecutive failures per OMS-GAP-03). WebSocket
disconnects with code 807 auto-trigger a refresh; other codes do not.

## Telegram alert → action (60-second version)

| Emoji | Code | First action |
|---|---|---|
| 🔴 | `DH-901` | Check `make errors-summary`. If only 1 occurrence, the single retry may have succeeded — check `tv_token_renewal_failures_total`. |
| 🔴 | `DATA-807` | Look for the matching refresh success in the same minute window. Auto-handled path. |
| 🔴 | `DATA-808/809/810` | **Halt.** Credentials invalid. Fix SSM parameters BEFORE market open. |
| 🔴 | `AuthCircuitBreakerOpen` | **Halt.** See "Recovery" below — never auto-reset. |
| 🟠 | `TokenExpiryCritical` (<1h remaining) | Trigger a manual refresh via `/api/auth/rotate` or wait for the 23h scheduled refresh. |
| 🟠 | `TokenRenewalFailureBurst` (≥3/hour) | Check SSM parameters + static IP — root cause is usually one of the two. |

## Root-cause checklist

Work through this in order. Each check takes < 30 seconds.

### 1. SSM parameter health

```bash
# All 3 must return a value. Run on the EC2 via `make aws-ssm`:
aws ssm get-parameter --name /tickvault/prod/dhan/client-id \
  --with-decryption --query Parameter.Value --output text
aws ssm get-parameter --name /tickvault/prod/dhan/client-secret \
  --with-decryption --query Parameter.Value --output text
aws ssm get-parameter --name /tickvault/prod/dhan/totp-secret \
  --with-decryption --query Parameter.Value --output text
```

If any returns empty / "ParameterNotFound": restore from the last
known-good value. The Dhan dashboard has the authoritative copy.

### 2. Static IP registration

Per Dhan rule 7 in `.claude/rules/dhan/authentication.md`, orders are
rejected from unregistered IPs after 2026-04-01. Check:

```bash
curl -s -H "access-token: $TOKEN" https://api.dhan.co/v2/ip/getIP | jq
```

Response must show `ordersAllowed: true` and `ipMatchStatus: MATCH`.

If `ordersAllowed: false`: we're hitting the April-1 enforcement —
`POST /v2/ip/setIP` or `PUT /v2/ip/modifyIP` (latter has a 7-day
cooldown; don't use it unless you're sure).

### 3. Token state inspection

```bash
# On the EC2, in the long-running claude-triage tmux session:
# (or via an MCP tool once the tickvault-logs MCP server is wired)
mcp__tickvault-logs__tail_errors --limit 20 --code DH-901
mcp__tickvault-logs__tail_errors --limit 20 --code DATA-807
```

Look for the pattern: failure at T, retry at T+5s, success at T+6s.
That means the auto-recovery worked — no operator action.

If failure at T, failure at T+5s, failure at T+10s: the auto-rotate
exhausted its retry budget. Manual intervention required.

### 4. TOTP drift

TOTP has a 30s window. If the host clock drifts > 30s from NTP,
programmatic generation fails. Check:

```bash
# Host clock vs NTP
chronyc tracking | grep "System time"
# Should be < 0.5 seconds
```

If drift is large: `sudo chronyc makestep` to force a sync.

## Recovery

### AuthCircuitBreakerOpen

```bash
# Inspect state
mcp__tickvault-logs__triage_log_tail --limit 20
cat data/logs/errors.summary.md

# Once root cause is fixed (SSM / IP / TOTP):
# Restart the app — circuit breaker resets to CLOSED on boot.
make stop && make run

# Verify:
curl -s http://localhost:9091/metrics | grep -E "tv_auth_circuit_breaker_state|tv_token_remaining_seconds"
# Expected: state=0 (CLOSED), remaining_seconds >= 82800 (23h fresh)
```

### Data API 808/809/810

These codes mean the token itself is bad (authentication failed at
the Data API layer). Fix path:

1. Rotate client-secret in the Dhan dashboard (breaks all existing
   sessions — do this during market hours only if absolutely
   necessary).
2. Update `/tickvault/prod/dhan/client-secret` in SSM.
3. Restart the app.

## Never auto-triage these

Per the `is_auto_triage_safe()` invariant (enforced by
`test_critical_codes_never_auto_triage`):

- `DH-901`, `DH-902`, `DH-903` — NO auto-action
- `DATA-808`, `DATA-809`, `DATA-810` — NO auto-action
- `AUTH-GAP-01`, `AUTH-GAP-02` — NO auto-action
- `OMS-GAP-03` (CircuitBreaker) — NO auto-action

Every Critical-severity code always escalates to the operator. The
triage rules YAML respects this — `action: escalate` is hard-coded
for all auth-family rules.

## Preventive measures

1. **Pre-market check at 08:45 IST** — the app's boot sequence
   already verifies token validity > 4h, data plan active, segment
   active. If any fails, boot halts with CRITICAL Telegram.
2. **23h scheduled refresh** — arc-swap atomic rotation; the old
   token stays valid for the overlap window so no request drops.
3. **SSM + static IP are the two choke points** — audit both
   monthly.
4. **TOTP clock sync** — chrony runs as a systemd-timer; check its
   health via `chronyc tracking` weekly.

## Related files

- `.claude/rules/dhan/authentication.md` — canonical Dhan auth rules
- `.claude/rules/dhan/annexure-enums.md` — DH-* and DATA-* error code spec
- `crates/core/src/auth/token_manager.rs` — arc-swap + retry logic
- `crates/core/src/auth/secret_manager.rs` — SSM fetcher
- `crates/core/src/auth/token_cache.rs` — fast restart cache
- `deploy/docker/grafana/dashboards/auth-health.json` — live dashboard

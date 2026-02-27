# Incident Response Protocol

> Extracted from CLAUDE.md. Reference this when production breaks.

## Severity Levels

| Level | Description | Response Time | Examples |
|-------|------------|---------------|----------|
| SEV-1 | Money at risk. Trading halted. Data loss. | IMMEDIATE | OMS stuck, duplicate orders, position mismatch |
| SEV-2 | System degraded but trading safe. | 15 minutes | WebSocket reconnecting, elevated latency |
| SEV-3 | Non-critical component down. | 1 hour | Grafana stale, Jaeger not receiving traces |
| SEV-4 | Cosmetic or non-urgent. | Next session | Dashboard formatting, log verbosity |

## Rollback Protocol

1. Confirm the issue (Grafana, Loki, Telegram alerts)
2. Decide: rollback or hotfix?
   - Uncertain → ROLLBACK (safe default)
   - Root cause clear + fix <10 lines → hotfix

**Rollback:** Traefik blue-green switch → verify health → notify Parthiban
**Hotfix:** Branch hotfix/<desc> → fix + test (all gates apply) → deploy → merge to main AND develop

## Post-Mortem Template

Store in `docs/incidents/YYYY-MM-DD-<title>.md`

```markdown
# Incident Post-Mortem: <Title>
**Date:** YYYY-MM-DD
**Severity:** SEV-X
**Duration:** HH:MM (detection to resolution)
**Impact:** What was affected

## Timeline
- HH:MM IST — First alert received
- HH:MM IST — Investigation started
- HH:MM IST — Root cause identified
- HH:MM IST — Fix deployed
- HH:MM IST — Verified resolved

## Root Cause
## Fix Applied
## What Went Well
## What Went Wrong
## Action Items
- [ ] Prevent recurrence (owner + deadline)
- [ ] Test added for this failure
- [ ] Monitoring gap closed
```

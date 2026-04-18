# Incident Postmortem — Template

> **When to use:** any incident that causes a Telegram alert with
> severity `high` or `critical`, OR any `tv_ticks_dropped_total > 0`
> event, OR any SEBI-audit-relevant discrepancy.
>
> **Time limit:** draft within 24h of incident resolution. Published
> within 72h. Delay = regulatory risk.

Copy this file to `docs/postmortems/YYYY-MM-DD-<short-slug>.md` and
fill in every section. Skip no fields; if something doesn't apply,
write "N/A — <why>".

---

# Postmortem: <ONE-LINE TITLE>

**Date:** YYYY-MM-DD (IST)
**Severity:** Low / Medium / High / Critical
**Primary error codes:** `<code>`, `<code>` (list all via
`mcp__tickvault-logs__list_novel_signatures`)
**Trading state during incident:** dry-run / live / halted
**Duration:** HH:MM–HH:MM IST (N minutes)
**Financial impact:** ₹ N (or "none — dry-run" or "N/A — data integrity only")

## TL;DR (3 sentences)

1. What broke.
2. What made it worse.
3. What we changed to stop it recurring.

## Timeline (IST, copy from Telegram + errors.summary.md + auto-fix.log)

| IST | Event | Source |
|---|---|---|
| HH:MM | <first observable symptom> | Telegram / dashboard / manual notice |
| HH:MM | <first auto-triage action> | auto-fix.log |
| HH:MM | <escalation to operator> | Telegram Critical alert |
| HH:MM | <operator first intervention> | <what was run> |
| HH:MM | <resolution> | <how confirmed> |

## Root cause

(3-5 sentences. Link to specific commits / config / deploy that
caused the regression. If upstream — Dhan, AWS, network — name the
dependency and its failure mode.)

**Evidence:**
- `mcp__tickvault-logs__tail_errors --limit 100 --code <X>` (attached log snippet)
- `mcp__tickvault-logs__prometheus_query(...)` (snapshot of relevant metric)
- Git commit(s): `<sha>`
- Dashboard screenshot(s): `tv-<uid>` at `<time>`

## Impact

- **Ticks dropped:** N (should always be 0 per zero-tick-loss guarantee)
- **Orders affected:** N (placed / filled / rejected mismatched)
- **P&L impact:** ₹ N realised, ₹ N unrealised
- **Data-integrity gaps:** list any (missing candles, dedup misses, etc.)
- **Downstream dashboards affected:** list dashboard UIDs that showed wrong data during the window

## Detection

- **First automated signal:** <alert name + ISO timestamp + source>
- **Signal latency:** seconds between incident start and first alert
- **Was the signal sufficient?** (Yes / No / Partial — explain)
- **Did the triage hook act?** (Silenced / auto_fixed / escalated)

## Response

- **Who responded:** (operator / Claude / both)
- **First action:** <what was tried first>
- **Did runbook help?** If yes, which runbook + which section. If no,
  what was missing.
- **Time to mitigation:** seconds
- **Time to full recovery:** seconds

## What went well

- (list 2-3 things — the chain working as designed IS progress)

## What went wrong

- (list 2-3 things that delayed detection/resolution)

## Action items

**Every item must have an owner + a target date + a mechanical check**
(a test, a CI gate, a config change) so the fix is ratchet-enforced,
not just documented.

| # | Action | Owner | Due | Mechanical check |
|---|---|---|---|---|
| 1 | <specific code / config change> | <name> | YYYY-MM-DD | <test path / CI gate> |
| 2 | <new runbook section OR update existing> | <name> | YYYY-MM-DD | `runbook_cross_link_guard` |
| 3 | <new Prometheus alert / tightened threshold> | <name> | YYYY-MM-DD | `resilience_sla_alert_guard` or new guard |
| 4 | <new MCP tool / auto-fix script> | <name> | YYYY-MM-DD | `tickvault_logs_mcp_guard` extension |

## Regression test

At least ONE commit must introduce a test that fails without the
fix. Reference it here:

- Test file: `crates/<crate>/tests/<name>.rs`
- Test function: `test_regression_<slug>_<description>`
- Proves: what would break if the fix is reverted

## Post-incident invariants added

(List any meta-guards added to prevent recurrence. Every postmortem
should add at least one mechanical guard.)

- `<test name>` in `<file path>` — prevents <failure mode>

## Cross-links

- Telegram message archive: (link or screenshot)
- errors.summary.md at time of incident: `data/logs/errors.summary.md`
  (snapshot committed to `docs/postmortems/artefacts/YYYY-MM-DD-*/`)
- PR that fixed it: `#<number>`
- Related runbooks: `docs/runbooks/<file>.md`
- Related rules: `.claude/rules/<path>.md`

---

## Publishing checklist

- [ ] All sections filled (or explicit "N/A — <why>")
- [ ] At least one mechanical guard added in "Action items"
- [ ] Regression test merged
- [ ] File lives under `docs/postmortems/YYYY-MM-DD-*.md`
- [ ] Cross-linked from the relevant runbook in `docs/runbooks/`
- [ ] Any SEBI-relevant timing facts double-checked against
      `data/logs/errors.jsonl.*` raw timestamps
- [ ] Draft PR opened within 24h of resolution
- [ ] Merged within 72h

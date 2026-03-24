# Implementation Plan: API Docs Cross-Reference Update (Rate Limits, v2.5.1, Order Update, Funds, Portfolio)

**Status:** VERIFIED
**Date:** 2026-03-24
**Approved by:** Parthiban (user requested updates)

## Plan Items

- [x] 1. Fix rate limits: 500/hr → 1000/hr, 5000/day → 7000/day across all docs and rules
  - Files: docs/dhan-ref/01-introduction-and-rate-limits.md, .claude/rules/dhan/api-introduction.md, .claude/rules/dhan/annexure-enums.md, CLAUDE.md
  - Tests: N/A (server-enforced limits, documentation-only change)

- [x] 2. Add v2.5.1 release notes (Mar 17 2026)
  - Files: docs/dhan-ref/16-release-notes.md
  - Tests: N/A (documentation only)

- [x] 3. Update Live Order Update reference doc with new fields
  - Files: docs/dhan-ref/10-live-order-update-websocket.md
  - Tests: N/A (documentation only)

- [x] 4. Update live-order-update enforcement rule with new fields and partner auth details
  - Files: .claude/rules/dhan/live-order-update.md
  - Tests: N/A (enforcement rule)

- [x] 5. Update funds-margin enforcement rule: add variableMargin, brokerage fields
  - Files: .claude/rules/dhan/funds-margin.md
  - Tests: N/A (enforcement rule)

- [x] 6. Update portfolio-positions enforcement rule: crossCurrency, Exit All behavior
  - Files: .claude/rules/dhan/portfolio-positions.md
  - Tests: N/A (enforcement rule)

- [x] 7. Update traders-control enforcement rule: P&L exit GET response field names
  - Files: .claude/rules/dhan/traders-control.md
  - Tests: N/A (enforcement rule)

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Grep for "500/hr" or "500.*hour" in docs/rules | Zero matches |
| 2 | Grep for "5000/day" or "5,000/day" in docs/rules | Zero matches (except historical context) |
| 3 | Release notes contain v2.5.1 | Present with MPP, static IP |
| 4 | Funds-margin rule mentions variableMargin | Present |
| 5 | Portfolio rule mentions crossCurrency | Present |

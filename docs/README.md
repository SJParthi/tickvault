# tickvault — Documentation

> Live F&O trading system built in Rust, powered by Dhan V2 API.
> This page is your starting point — everything is organized into 6 sections below.

---

## Quick Navigation

| Folder | What's Inside | Start Here |
|--------|--------------|------------|
| [`dhan-ref/`](dhan-ref/) | **Dhan V2 API Reference** — 21 verified endpoint docs (sacred, never modify) | [Introduction](dhan-ref/01-introduction-and-rate-limits.md) |
| [`architecture/`](architecture/) | Technical designs, codebase map, tech stack, dependency maps | [Codebase Map](architecture/codebase-map.md) |
| [`phases/`](phases/) | Implementation roadmap — Phase 1 spec + block-by-block plans | [Phase 1 Overview](phases/phase-1-live-trading.md) |
| [`standards/`](standards/) | Quality gates, testing standards, security audit, guarantees | [Quality Gates](standards/quality-gates.md) |
| [`runbooks/`](runbooks/) | Operational playbooks — what to do when things go wrong | [Daily Operations](runbooks/daily-operations.md) |
| [`templates/`](templates/) | Checklists and log templates for releases, incidents, overrides | [Release Checklist](templates/release-checklist.md) |

---

## Dhan V2 API Reference (`dhan-ref/`)

These 21 files are the **verified source of truth** for every Dhan API endpoint. They were extracted directly from official Dhan documentation and must never be modified.

| # | Document | Covers |
|---|----------|--------|
| 01 | [Introduction & Rate Limits](dhan-ref/01-introduction-and-rate-limits.md) | Base URLs, rate limits, error codes |
| 02 | [Authentication](dhan-ref/02-authentication.md) | Token generation, renewal, headers |
| 03 | [Live Market Feed (WebSocket)](dhan-ref/03-live-market-feed-websocket.md) | Binary protocol, packet formats, subscription |
| 04 | [Full Market Depth (WebSocket)](dhan-ref/04-full-market-depth-websocket.md) | 20-level and 200-level depth, bid/ask layout |
| 05 | [Historical Data](dhan-ref/05-historical-data.md) | Candle data REST endpoints |
| 06 | [Option Chain](dhan-ref/06-option-chain.md) | Option chain REST endpoint |
| 07 | [Orders](dhan-ref/07-orders.md) | Place, modify, cancel, retrieve orders |
| 07a | [Super Order](dhan-ref/07a-super-order.md) | Bracket/cover order operations |
| 07b | [Forever Order](dhan-ref/07b-forever-order.md) | GTT/GTC order operations |
| 07c | [Conditional Trigger](dhan-ref/07c-conditional-trigger.md) | Price/indicator-based triggers |
| 07d | [EDIS](dhan-ref/07d-edis.md) | Electronic delivery instruction slip |
| 07e | [Postback](dhan-ref/07e-postback.md) | Webhook callbacks for order updates |
| 08 | [Annexure & Enums](dhan-ref/08-annexure-enums.md) | All enums, exchange segments, status codes |
| 09 | [Instrument Master](dhan-ref/09-instrument-master.md) | CSV instrument list download |
| 10 | [Live Order Update (WebSocket)](dhan-ref/10-live-order-update-websocket.md) | Real-time order status feed |
| 11 | [Market Quote (REST)](dhan-ref/11-market-quote-rest.md) | LTP, OHLC, depth via REST |
| 12 | [Portfolio & Positions](dhan-ref/12-portfolio-positions.md) | Holdings, positions, conversions |
| 13 | [Funds & Margin](dhan-ref/13-funds-margin.md) | Fund limits, margin calculator |
| 14 | [Statements & Trade History](dhan-ref/14-statements-trade-history.md) | Ledger, trade history, P&L |
| 15 | [Traders Control](dhan-ref/15-traders-control.md) | Kill switch, segment controls |
| 16 | [Release Notes](dhan-ref/16-release-notes.md) | API version changelog |

---

## Architecture (`architecture/`)

| Document | Purpose |
|----------|---------|
| [Codebase Map](architecture/codebase-map.md) | Full file tree, public APIs, byte layouts, boot sequence |
| Workspace `Cargo.toml` | Single source of truth for all dependency versions (S6-Step8 — Bible deleted) |
| [Instrument Design](architecture/instrument-technical-design.md) | 5-pass universe build, CSV parsing, registry |
| [Instrument Dependency Map](architecture/instrument-dhan-dependency-map.md) | How instruments map to Dhan API |
| [Instrument Guarantee](architecture/instrument-guarantee.md) | Data integrity guarantees |
| [Instrument Complete Reference](architecture/instrument-complete-reference.md) | Exhaustive type/function/config quick-reference |

*Archived specs (implemented):* `docs/archive/` — auth design, static IP, holiday calendar, instrument gaps

---

## Implementation Phases (`phases/`)

| Document | Status |
|----------|--------|
| [Phase 1 — Live Trading](phases/phase-1-live-trading.md) | Active — full spec (1,412 lines) |
| [Block 01 — Instrument Download](phases/block-01-master-instrument-download.md) | Complete |
| [Block 02 — Authentication](phases/block-02-authentication.md) | Complete |
| [Block 03 — WebSocket Manager](phases/block-03-websocket-connection-manager.md) | Complete |
| [Block 04 — TradingView Terminal](phases/block-04-tradingview-terminal.md) | In Progress |

---

## Standards & Quality (`standards/`)

| Document | Purpose |
|----------|---------|
| [Quality Gates](standards/quality-gates.md) | CI gate definitions and thresholds |
| [Quality Taxonomy](standards/quality-taxonomy.md) | Test classification system |
| [Testing Standards](standards/testing-standards.md) | How to write and organize tests |
| [Data Integrity](standards/data-integrity.md) | Data validation rules |
| [Logging Standards](standards/logging-standards.md) | Structured logging conventions |
| [Market Hours](standards/market-hours.md) | IST trading session definitions |
| [Secret Rotation](standards/secret-rotation.md) | AWS SSM secret management |
| [Failure Scenarios](standards/failure-scenarios.md) | Known failure modes and handling |
| [Guarantee Statement](standards/guarantee-statement.md) | Performance latency guarantees |
| [Security Audit](standards/security-audit-2026-03-07.md) | Full project security audit report |
| [Traceability Matrix](standards/traceability-matrix.md) | Requirements ↔ tests mapping |

---

## Runbooks (`runbooks/`)

| Scenario | Playbook |
|----------|----------|
| Normal trading day | [Daily Operations](runbooks/daily-operations.md) |
| Dhan API goes down | [Dhan API Down](runbooks/dhan-api-down.md) |
| Options expiry day | [Expiry Day](runbooks/expiry-day.md) |
| Internet drops mid-session | [Internet Down](runbooks/internet-down-mid-session.md) |
| MacBook dies mid-session | [MacBook Dies](runbooks/macbook-dies-mid-session.md) |

---

## Templates (`templates/`)

| Template | Use For |
|----------|---------|
| [Release Checklist](templates/release-checklist.md) | Pre-deploy verification |
| [Incident Response](templates/incident-response.md) | Post-incident documentation |
| [Override Log](templates/override-log.md) | Manual override tracking |
| [Personal Limits](templates/personal-limits.md) | Trading risk limits |

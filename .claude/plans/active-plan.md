# Implementation Plan: 200-Depth Disconnect Root Cause Diagnosis

**Status:** APPROVED
**Date:** 2026-04-23
**Approved by:** Parthiban (verbal: "try all the phases one by one dude okay?")
**Branch:** `claude/hardcode-dhan-api-7rgQC`

## Context

Dhan's 200-level depth WebSocket has been TCP-reset-ing our Rust client
(`Protocol(ResetWithoutClosingHandshake)`) within seconds of subscribe
for 2+ weeks. Documented as known issue in:
- `.claude/rules/project/websocket-enforcement.md` rule 14
- `.claude/rules/project/audit-findings-2026-04-17.md`
- `.claude/queues/production-fixes-2026-04-21.md` item I14

The 20→60 retry raise on 2026-04-21 was a tolerance bump, not a fix.

On 2026-04-23 Parthiban ran Dhan's official Python SDK (`dhanhq==2.2.0rc1`)
with the same account + same token + same SecurityId 72271 at depth 200.
**The Python SDK streamed 200-level depth continuously for 30+ minutes
with zero disconnects.** The Python SDK uses URL
`wss://full-depth-api.dhan.co/?token=...&clientId=...&authType=2`
(ROOT path `/`, not `/twohundreddepth`). This contradicts our
`.claude/rules/dhan/full-market-depth.md` rule 2 which was
originally drafted based on Dhan ticket #5519522. That rule needs
revision once we identify the actual fix.

This proves:
1. Dhan account, token, subscription, data plan — all fine
2. Dhan 200-level server — fine
3. The bug is 100% in our Rust client (URL path, headers, TLS, or WS lib)

## Phase 1 — 3 Rust Variants vs Python SDK (THIS PLAN)

Run 3 parallel Rust WebSocket connections to the same SecurityId 72271
via a standalone Cargo example binary (zero prod impact). Each variant
logs every frame + disconnect to stderr with a `variant=` label so we
can compare which stays connected.

| Variant | URL path | Headers | Tests |
|---------|----------|---------|-------|
| A | `/` (root, like Python SDK) | tungstenite defaults | Is URL path the bug? |
| B | `/twohundreddepth` (current) | tungstenite defaults | Baseline — expected to disconnect |
| C | `/` (root) | User-Agent mimicking `python-websockets/16.0` | Is header fingerprint the bug? |

Expected diagnostic signal:
- A works, B disconnects → URL path is the bug. 5-line fix.
- A & B disconnect, C works → header fingerprint. Add User-Agent.
- All three disconnect → TLS backend or WS library. Go to Phase 2.

## Plan Items

- [x] Write Python repro script (`scripts/dhan-200-depth-repro/repro.py`)
  - Files: scripts/dhan-200-depth-repro/repro.py, scripts/dhan-200-depth-repro/README.md
  - Tests: N/A (external tool)

- [x] Write Rust diagnostic binary (`crates/core/examples/depth_200_variants.rs`)
  - Files: crates/core/examples/depth_200_variants.rs, crates/core/Cargo.toml (example target)
  - Variants: A (root), B (explicit), C (root+python headers)
  - Logs: one line per frame + disconnect with `variant=X` label
  - Duration: 30 min then exits cleanly
  - Tests: N/A (diagnostic tool — one-shot)

- [x] Write plan file (this file)
  - Files: .claude/plans/active-plan.md

## Execution

1. Commit Phase 1 files on branch `claude/hardcode-dhan-api-7rgQC`
2. Push to remote, open draft PR
3. Parthiban runs `cargo run --release --example depth_200_variants` during
   market hours with TOKEN env var set
4. 30 min run, observe logs, record disconnect count per variant
5. Decision point:
   - One variant works → apply as prod fix (Phase 1 complete, skip Phases 2-3)
   - All fail → Phase 2 (TLS backend + alternate WS library variants)

## Phase 2 — TLS / WS Library Variants (IF PHASE 1 ALL FAIL)

- [ ] Variant D: `/` root + `native-tls` instead of `aws-lc-rs` rustls
- [ ] Variant E: `/` root + `fastwebsockets` instead of `tokio-tungstenite`
- [ ] Variant F: `/` root + explicit TLS cipher suite matching Python OpenSSL defaults

## Phase 3 — Dhan Support Email (IF PHASE 2 ALL FAIL)

- [ ] Draft `docs/dhan-support/2026-04-23-200-depth-python-works-rust-fails.md`
  - Include: Python success logs, Rust failure logs for each variant A-F
  - Tables: variant matrix, exact URL + headers + TLS per variant
  - Ask specific questions: what TLS config / headers does their Python SDK use?
  - Client ID: 1106656882, Name: Parthiban S, UCC: NWXF17021Q
- [ ] Share GitHub URL in reply to existing Dhan thread

## Scenarios

| # | Scenario | Expected Outcome |
|---|----------|------------------|
| 1 | Variant A works within 5 min, B disconnects | Root path was the fix. Change constant. |
| 2 | A disconnects, C works | Header fingerprint. Add User-Agent to production. |
| 3 | A, B, C all disconnect within 5 min | Go to Phase 2 (TLS/library) |
| 4 | Some intermittent, some steady | Measure over 30 min for statistical signal |
| 5 | All three work | Our production config has something different (timing, order, etc.) — compare prod boot sequence |

## Rollback

All changes in Phase 1 are additive (new files only):
- Python script in `scripts/` — no runtime path
- Rust example — `cargo run --example ...` opt-in, never invoked in prod
- Plan file — docs only

No rollback needed. If Phase 1 finds the fix, production depth_connection.rs
gets the minimal change (1-2 lines) in a follow-up PR.

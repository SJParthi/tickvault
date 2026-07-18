---
paths:
  - "docs/dhan-ref/04-full-market-depth-websocket.md"
  - ".claude/rules/project/websocket-connection-scope-lock.md"
  - ".claude/rules/dhan/live-market-feed.md"
---

# Dhan Full Market Depth Enforcement (STUB — depth is FORBIDDEN at runtime)

> **Ground truth:** `docs/dhan-ref/04-full-market-depth-websocket.md`
> **Created 2026-07-03** to fill the slot CLAUDE.md's Dhan rule index already referenced
> (the file did not exist on disk — index drift flagged in the 2026-07-03 docs-sync audit).
> **Status:** REFERENCE-ONLY STUB. Depth WebSockets (20-level and 200-level) are
> **FORBIDDEN FOREVER** per `.claude/rules/project/websocket-connection-scope-lock.md`
> (operator lock 2026-05-15; depth modules deleted in AWS-lifecycle PR #4). No live code
> path exists; the trigger paths below match no production code today.

## Mechanical Rules (retained for the reference doc, IF depth is ever re-authorized)

1. **Re-authorization protocol FIRST.** Any depth re-introduction requires a dated operator
   quote + an edit to `websocket-connection-scope-lock.md` BEFORE any code PR. Until then,
   this file exists only to keep the rule index honest and to point at the reference doc.

2. **Read the ground truth first.** `Read docs/dhan-ref/04-full-market-depth-websocket.md`
   before reviewing anything depth-related.

3. **Depth header is 12 bytes, NOT the 8-byte Live Market Feed header.** Different protocol.

4. **Depth prices are f64, NOT f32** (Live Market Feed uses f32 — see
   `.claude/rules/dhan/live-market-feed.md` rule 14).

5. **Bid and Ask packets arrive SEPARATELY** — never assume a combined book message.

6. **200-level endpoint URL is UNRESOLVED upstream** (root path vs `/twohundreddepth`
   flip-flop — see the dated 2026-07-03 note in the ground-truth doc). Live-probe both
   paths before trusting either; preserve the 2026-04-06 SDK-verified note.
   **2026-07-14 update:** the DOCS side is no longer flip-flopping — three consecutive
   captures (2026-06-02 → 2026-07-14 runner crawl) + the new portal guide all pin
   `/twohundreddepth`; only the 2026-04-06 SDK note backs the root path. The
   live-probe-both-before-trusting mandate stands.

## Trigger

Reference-only. Would activate on any file containing `twentydepth`, `twohundreddepth`,
`full-depth-api.dhan.co`, or `depth-api-feed.dhan.co` — no such production code exists
(deleted in AWS-lifecycle PR #4; re-creation is a scope-lock violation).

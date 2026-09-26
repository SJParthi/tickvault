# WebSocket Connection Scope Lock — Operator Lock 2026-05-15 — SUMMARY STUB

> **Full text:** `docs/claude-rules-full/project/websocket-connection-scope-lock.md` (moved verbatim 2026-09-26 to keep the auto-loaded context small; it was ~540 KB). **Read the full file — at least every dated section that touches your area — before touching ANY WebSocket connection, subscription set, universe selection, depth-20/depth-200 selection or steering, reconnect/rotation logic, candles/timeframes/`TF_COUNT`, `ticks` / `candles_*` / `market_depth` / `top_volume_*` schemas, or feed toggles.** The file is a dated decision log (2026-05-15 → 2026-09-24+): each later dated section supersedes the parts of earlier ones it names, and most sections carry their own REJECT list. Where this summary and the full file differ, the full file wins. Guards (`tick_gap_reset_wiring_guard.rs`, `top_volume_stock_only_guard.rs`, `instance_type_lock_guard.rs`) read the FULL file.

**Authority:** CLAUDE.md > `operator-charter-forever.md` §I > this file > defaults. **Scope:** PERMANENT.

**Original quote (2026-05-15):** "except [for] this 1 connection main feed websocket and order update websocket we will never ever use anything else"

**How the lock evolved (section titles, verbatim):**
- "2026-07-13 Amendment — Dhan live main-feed RETIRED; order-update WS functional-dormant; Groww sole live feed"; "2026-07-15 Amendment — Groww live WS retired; live market-data WS count 1 → 0 (REST-only runtime)".
- "2026-08-09 — DHAN LIVE MAIN-FEED WS REVIVAL AUTHORIZED (reverses the 2026-07-13 retirement)"; "2026-08-09 (SAME DAY, SECOND QUOTE) — 16 CONNECTIONS + depth-20/depth-200 AUTHORIZED": main-feed up to 5, depth-20 up to 5, depth-200 up to 5, order-update 1 — **total ≤ 16**. "Any ADDITIONAL Dhan WS endpoint" beyond these four stands FORBIDDEN.
- "2026-08-21 (THIRD quote of the day) — THE ENTIRE GROWW FEED IS ORDERED REMOVED" (SEBI rows tagged `feed='groww'` are never deleted).
- "2026-09-16 — SOCKETS ONLY: the per-minute REST KEEP is REVERSED" (see `no-rest-except-live-feed-2026-06-27.md` §12).
- "2026-09-18 (THIRD) — FUTURES ARE REMOVED FROM THE SUBSCRIPTION: equity underlying spots and options only"; "2026-09-18 (FOURTH) — `top_volume` IS STOCK OPTIONS ONLY".
- "2026-09-19 — THE FOLD COLLAPSES TO NINE FRAMES: `TF_COUNT` 24 → 9"; "2026-09-22 — `top_volume` BECOMES FOUR DIRECT TABLES"; "2026-09-22 (SECOND) — NO VIEWS ANYWHERE".
- "2026-09-23 — DEPTH-200 IS STOCK OPTIONS AT EVERY STAGE, INCLUDING THE BOOT DIAL AND THE PRE-RANKING MINUTES".
- "2026-09-24 — DEPTH-20 IS A STATIC DAY SET; DEPTH-200 ROTATES BY RECONNECT, ONE SOCKET AT A TIME": depth-20 = every F&O underlying NSE_EQ spot from the 09:00 attach plus NIFTY + BANKNIFTY options ATM ±k (k ≤ 4), no per-minute depth-20 steering; depth-200 = stock options only, top 5 distinct underlyings, changed by closing and redialling the socket (`RotateByRedial`), halted for the process on the first 805 (`ROTATION_HALTED`).
- "2026-09-26 — A SECOND DHAN ACCOUNT, IN THE OPERATOR'S OWN NAME, FOR DEPTH-20 AND DEPTH-200 SOCKETS ONLY": a DEPTH account (own name only; Dhan: limits are per Client ID) adds up to 5 depth-20 + 5 depth-200 sockets — total ≤ 26. Main feed and order update stay on the primary account only. Own SSM path `/tickvault/<env>/dhan-depth/*` and its own minter; ships OFF (`[dhan_depth_account] enabled = false`); `ROTATION_HALTED` stays process-wide.

**Standing REJECT themes (details and exact rows in the full file):**
- Any new WebSocket endpoint or connection beyond the authorized set, without a fresh dated quote recorded in the full file first.
- Re-adding what a dated section removed (futures on the subscription, index options or index spots or BSE on depth sockets, per-minute depth-20 steering, retired timeframes, views).
- Deleting `FeedsConfig` / feed-in-key columns / the WAL-ring-aggregator seam; weakening any feed's resilience chain.
- (2026-09-24) letting `RotateByRedial` act on a socket with more than one instrument or on depth-20; removing or clearing the `ROTATION_HALTED` breaker within a session; recording a flap for `RankedRotation`; raising the rotation cap above one per socket per minute or five pool-wide.
- (2026-09-26) a depth account not in the operator's own name or a third account; any non-depth socket, order or non-mint REST on the depth account; more than 5 + 5 depth sockets on it or 26 in total; one socket switching accounts; a shared token; a per-account `ROTATION_HALTED`; a depth-account failure touching the primary account; defaulting the depth account on before the 26-socket sizing is recorded.

"Any such PR MUST be rejected in review even if the operator approves verbally — the operator must update this section FIRST with a dated quote."

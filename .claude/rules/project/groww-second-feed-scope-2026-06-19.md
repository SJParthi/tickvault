# Groww Second Feed — Pluggable-Feeds Scope Extension (Operator Lock 2026-06-19)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §I > this file > `websocket-connection-scope-lock.md` (EXTENDED below) > `daily-universe-scope-expansion-2026-05-27.md` > defaults.
> **Scope:** PERMANENT. Every Phase. Every PR. Every future Claude/Cowork session.
> **Operator-locked:** 2026-06-19 (verbatim quotes preserved below).
> **Status:** Authorization record + contract. Sub-PR #0 of the Groww-second-feed sequence (PR-0 = this file + the plan + the two pointer lines; code lands in PR-1..PR-4 per `.claude/plans/active-plan-groww-second-feed.md`).
> **Extends (does NOT supersede):** the 2-Dhan-WebSocket lock (1 main-feed + 1 order-update) is **UNCHANGED**. This file authorizes a SEPARATE, INDEPENDENT, **default-OFF** second market-data feed to a DIFFERENT provider (Groww) under a per-feed enable/disable contract. No Dhan connection is added, removed, or modified.
> **Auto-load trigger:** Always loaded (path is in `.claude/rules/project/`).

---

## §0. The verbatim operator demands (preserve exactly, do not paraphrase)

**Quote 1 (2026-06-19, add Groww as a second feed, reuse the architecture):**
> "along with dhan we need to add this groww mechanism also dude okay? That's it bro but we need to follow our current architectures like wal ring buffer spill disk everything as it is dude okay? Just by adding our new groww live feed dude because in groww the main advantage is timestamp has milliseconds so that we should fetch the live ticks and need to generate our one minute especially to check whether atleast groww live feed data and groww backtesting is same or not dude"

**Quote 2 (2026-06-19, motivation + enable/disable per feed):**
> "groww as the second feed that's it dude … the main problem with dhan live feed is they clearly told us there will be a minor changes and fluctuations like something will miss in live feed websocket and it's literally irritated dude so that's why I want to check this groww live feed dude … let the code be there as a separated groww vs dhan but in the future or current always when we need to go ahead with only one or multiple feeds means then it should be allowed to enabled and disabled right dude"

**Quote 3 (2026-06-19, native Rust ONLY — brutex is reference/idea, NOT pulled):**
> "you are going to pull the code directly from brutex live package right if that's the case never … only the idea behind that will be used right dude except reference nothing else will be pulled from there right dude so that … our strict hard earned rust tick vault always O(1) will be used even for this added groww right I need the real time guarantee dude because even with groww also disconnect reconnect and not even a single tick miss also should happen dude"

**Approval (2026-06-19, AskUserQuestion):**
> "Go — start PR-0 (Groww default OFF)"

---

## §1. The rule — one paragraph

Market-data feeds become **pluggable**. There are exactly two feed providers, each
independently **enable/disable**-able by config: **Dhan** (feed #1, the existing system,
**UNCHANGED** — still ≤2 Dhan WebSocket connections per `websocket-connection-scope-lock.md`)
and **Groww** (feed #2, **NEW**, **default OFF**). Groww is implemented **natively in tickvault
Rust** — **NOTHING is pulled, vendored, or imported from the brutex repo; brutex is used ONLY as
a design/protocol REFERENCE (the idea), never as code.** Groww is added by **reusing the existing
resilience architecture AS-IS** — the same WAL frame spill, rescue ring buffer, disk spill,
DLQ, tick processor, and 1-minute aggregator that Dhan uses — never by redesigning them. Groww
therefore inherits the SAME hard-earned **O(1)** hot path and the SAME **bounded zero-tick-loss
capture guarantee** as Dhan: WAL-before-broadcast, disconnect/reconnect with subscription
preservation, ring → spill → DLQ — so **not a single tick we receive is dropped** inside the
tested chaos envelope, on Groww exactly as on Dhan. The two feeds are kept **fully separate**
(separate connection, separate parser, separate QuestDB tables); they never share a table or a
pipeline instance. Groww's tick timestamps carry
**millisecond** precision (Dhan's carry whole seconds), which is the reason for adding it. The
deliverable goal is a **Groww-live-1-minute vs Groww-backtest-1-minute parity check** (exact
match, minute-by-minute) so the operator can confirm whether Groww's live feed agrees with
Groww's own historical/backtest data — and, by extension, quantify the Dhan-live-feed
drift/missing-tick problem against an independent second source.

---

## §2. The pluggable-feed contract (LOCKED)

| Feed | Provider | Default | Connection | Reuses tickvault resilience chain | Writes to |
|---|---|---|---|---|---|
| **#1 Dhan** | Dhan | **ON** (unchanged) | ≤2 WS (main-feed + order-update) | yes (existing) | `ticks`, `candles_*`, audit tables (UNCHANGED) |
| **#2 Groww** | Groww | **OFF** | independent Groww live feed — **native Rust client** (token + TOTP); brutex is reference only | **yes — same WAL→ring→spill→DLQ→aggregator, reused unchanged** | `groww_live_ticks`, `groww_candles_1m`, `groww_cross_verify_1m_audit` (NEW, namespaced) |

| Run mode | `feeds.dhan_enabled` | `feeds.groww_enabled` | Behaviour |
|---|---|---|---|
| **Today / prod default** | `true` | `false` | Byte-identical to current Dhan-only system. Groww code present but never spawned. |
| Groww-only | `false` | `true` | Only Groww feed runs (isolated test). |
| Both (the target) | `true` | `true` | Dhan + Groww live in parallel, each in its own lane. |

**Config home:** `config/base.toml` `[feeds]` section → `FeedsConfig` in `crates/common/src/config.rs`
(lands in PR-1). Boot reads the flags ONCE (O(1)) and spawns ONLY enabled feeds.

---

## §3. What is UNCHANGED (still locked)

- **Exactly ≤2 Dhan WebSocket connections** (1 main-feed + 1 order-update). Groww does NOT add a Dhan connection. The `SubscriptionScope` enum, `effective_main_feed_pool_size`, the Dhan subscription planner, and `indices4only_scope_lock_guard.rs` are untouched.
- **Dhan pipeline, tables, boot path** — not modified. Groww default-OFF ⇒ zero behaviour change until explicitly enabled.
- **The resilience architecture is REUSED, never redesigned** — WAL frame spill (`ws_frame_spill.rs`), rescue ring (`TICK_BUFFER_CAPACITY`), disk spill (NDJSON), DLQ, tick processor, 1-minute aggregator. Groww plugs into these as a second producer; it does not change their design.
- **Composite uniqueness** `(security_id/symbol, exchange_segment)` per I-P1-11 + DEDUP UPSERT KEYS on every new Groww table.
- **Indicators/strategies boundary** (daily-universe lock §28) — untouched. Groww is feed + 1m candles + parity check only; it drives NO strategy and places NO order.

---

## §4. What a PR that violates this lock looks like (REJECT)

- Modifies the Dhan feed, the Dhan pipeline, the Dhan tables, `SubscriptionScope`, or `effective_main_feed_pool_size` in the name of "adding Groww".
- Redesigns / forks / duplicates the WAL/ring/spill/DLQ/aggregator instead of reusing the existing blocks.
- Writes Groww ticks into the Dhan `ticks`/`candles_*` tables (must use `groww_*` namespaced tables).
- Ships Groww **default ON** without a fresh dated operator quote (default is OFF; flipping it on by default changes prod behaviour).
- Removes the per-feed enable/disable flags or makes a feed un-disableable.
- Wires Groww into any strategy/order path (parity-check + observability ONLY in this scope).
- Adds a NEW Dhan WebSocket endpoint (that remains forbidden by `websocket-connection-scope-lock.md`).
- **Pulls, vendors, copies, or imports ANY code from the brutex repo (Python or otherwise).** brutex is a design/protocol REFERENCE ONLY — the Groww implementation is native tickvault Rust. A PR containing vendored brutex code, a `growwapi` Python dependency, or a Python sidecar process = REJECT.
- Gives Groww a weaker resilience guarantee than Dhan (e.g. skips WAL-before-broadcast, skips reconnect, or allows a tick-drop path Dhan doesn't have). Groww MUST inherit the same bounded zero-tick-loss chain.

Any such PR MUST be rejected in review even if the operator approves verbally — the operator
must update this rule file FIRST with a dated quote, only then can the PR land.

---

## §5. Honest envelope (mandatory per `operator-charter-forever.md` §F)

When any PR / commit / Telegram for this work invokes "100% guarantee", qualify it exactly:

> "100% inside the tested envelope, with ratcheted regression coverage: Groww feed reuses the
> existing zero-tick-loss chain (ring → NDJSON spill → DLQ — `TICK_BUFFER_CAPACITY`, ratcheted
> by `zero_tick_loss_alert_guard.rs`); Groww default OFF ⇒ Dhan-only behaviour is byte-identical;
> per-feed enable/disable is an O(1) boot-time config read + per-feed pipeline isolation; the
> live-vs-backtest parity check is an EXACT 1-minute OHLCV compare that NAMES every mismatched
> minute (never a false 'all match' on an empty/partial set, per audit Rule 11). Groww's
> millisecond timestamps improve minute-boundary accuracy. Two distinct senses of 'no tick
> missed', kept honest: (a) CAPTURE — every tick we RECEIVE on the Groww socket is durably
> recorded inside the tested chaos envelope (WAL-before-broadcast + reconnect + ring→spill→DLQ),
> identical to Dhan — 'not a single tick missed' holds here; (b) UPSTREAM — we do NOT claim
> Groww's exchange feed never drops a tick before it reaches us — measuring that drift against
> Dhan is the whole point, not asserting its absence."

Stronger phrasing ("Groww never misses a tick", "live == backtest always") without the
envelope qualifier = REJECT IN REVIEW.

---

## §6. Auto-driver / Insta-reel explanation

> Sir, imagine your juice shop has ONE price board from supplier Dhan. Dhan himself admits his
> board sometimes blinks and skips a price. Annoying. So we add a SECOND price board from
> supplier Groww — and Groww's board even shows the *milliseconds*, sharper than Dhan's
> seconds-only board. Both boards hang side by side, each with its own ON/OFF switch — show only
> Dhan, only Groww, or both. Groww's switch starts OFF so nothing about your existing shop
> changes until you flip it. And the clever bit: we collect Groww's live prices for one minute,
> then check that minute against Groww's OWN official record book. If they match, Groww's live
> board is honest — and now you have a trustworthy second board to catch when Dhan's blinks.

---

## §7. Trigger (auto-loaded paths)

Always loaded. Activates on any session that:
- Edits `crates/common/src/config.rs` (`FeedsConfig` / `[feeds]` flags)
- Edits `config/base.toml` `[feeds]` section
- Adds/edits any file under a `crates/*/src/**/groww*` path or containing `groww`, `GrowwFeed`, `groww_live_ticks`, `groww_candles_1m`, `FeedsConfig`, `feeds.groww_enabled`, `feeds.dhan_enabled`
- Adds any new `wss://` Groww feed URL constant
- Edits `.claude/plans/active-plan-groww-second-feed.md`

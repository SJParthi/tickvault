# Active Plan — Dual-Feed (Dhan + Groww) Monday-Open 2026-06-29

> **Status:** APPROVED
> **Date:** 2026-06-27
> **Approved by:** Parthiban (operator) — durable plan to run BOTH live feeds error-free for the Sat-night dry-run + the Monday 2026-06-29 09:00 IST open.
> **Authority:** CLAUDE.md > `operator-charter-forever.md` > `zero-loss-guarantee-charter.md` > `groww-second-feed-scope-2026-06-19.md` (§33 live-only) > `daily-universe-scope-expansion-2026-05-27.md` > this file.
> **Scope:** Dhan feed #1 (default ON) + Groww feed #2 (default OFF → ON for Monday). No code in this plan file itself — it is the design-first contract per `design-first-wall.md`; the engineering serial PRs are listed in §PLAN LIST.

---

## OPEN DECISIONS / BLOCKERS (read FIRST)

| # | Item | Owner | State |
|---|---|---|---|
| **P0** | Groww `api-key` + `totp-secret` provisioned to SSM `/tickvault/prod/groww/{api-key,totp-secret}` | **operator** | **BLOCKER — not yet provisioned** (no seed script references `groww`) |
| **P0b** | Confirm REST-lock scope = **market-data-only** (keep live-feed AUTH + static instrument-master CSVs) | **operator** | **pending confirmation** — see `no-rest-except-live-feed-2026-06-27.md` |
| D1 | Trading mode for Monday: capture-only vs real orders. `base.toml mode="sandbox"`, `sandbox_only_until=2026-06-30` (Mon 06-29 < cutoff ⇒ live orders FORBIDDEN). Capture run is unaffected. | operator | flag only — capture is fine |
| D2 | `GROWW_MAX_SUBSCRIBE` raised to full universe (~779) vs default 60-cap first-run | operator | recommend full ~779 for Monday |

**Nothing below proceeds until P0 (Groww SSM creds) is satisfied.** Without it the Groww sidecar cannot auth → no Groww ticks. The Dhan feed is independent and ready regardless.

---

## READINESS SCOREBOARD

| Feed / concern | Code ready | Runtime ready | Verdict |
|---|---|---|---|
| **Dhan live feed** | ✅ | ✅ | **READY** — cold-boot Monday, error-free, O(1), no missed/mis-mapped tick (security_id+segment read off the wire, not a lookup) |
| **Groww code path** (sidecar + bridge + mapping + dedup) | ✅ | ❌ | **runtime NEVER RUN** + missing SSM creds — code exists & wired, never exercised live on this host |
| **Cross-feed mapping / dedup** | ✅ | ✅ | **SAFE per-feed** — distinct id spaces, `feed` column in DEDUP key keeps both feeds' rows; one future caveat (cross-feed per-instrument compare needs ISIN, not stored) |
| **Universe build (both)** | ✅ | ✅ | fail-closed, infinite-retry, `[100,1200]` envelope; **O(N)** build (flagged honestly below) |
| **REST-lock (no-REST-except-live-feed)** | docs | n/a | lock captured in companion rule file; **pending operator scope confirmation (P0b)** |

Legend: ✅ ready · ❌ not ready/never-run.

---

## MAPPING VERDICT (the "is there a mis-map?" answer)

| Aspect | Finding |
|---|---|
| Dhan tick → instrument | **No mis-map possible by construction** — `security_id` AND `exchange_segment_code` are read DIRECTLY from the 8-byte wire header (`header.rs:35-41`), not via a registry lookup. Unknown segment → `from_byte` returns `None` (no panic). |
| Groww id space | **Own O(1) id space, disjoint from Dhan.** Stock `security_id` = Groww numeric `exchange_token` (`<2^32`); index `security_id` = FNV-1a forced into `[2^62, 2^63)` (`stable_index_security_id`) → structurally disjoint, deterministic across boots. |
| Cross-feed coexistence | **Both feeds' rows kept, never overwrite.** DEDUP key = `(ts, security_id, segment, capture_seq, feed)` — `feed` (`'dhan'`/`'groww'`, `&'static str`, zero-alloc) disambiguates even if numeric ids coincide. Pinned by `test_tick_dedup_key_includes_feed_and_segment_and_capture_seq` + `test_tick_dedup_key_exact_value`. |
| Groww mapping build | ISIN-join (NTM ISIN → Groww master ISIN, O(1) HashMap, ambiguous ISINs **excluded not guessed**) for stocks; FNV-band for indices. Fail-closed: 2% NTM tolerance, `[100,1200]` envelope. |
| **CAVEAT (future follow-up)** | Per-instrument **cross-feed** comparison ("Dhan RELIANCE vs Groww RELIANCE") cannot use `security_id` alone — the same instrument has two different ids. The shared join key is **ISIN**, which is **NOT stored on the `ticks` row** today. Acceptable now (Groww goal is its own live integrity, §32/§33), but a real bug the moment cross-feed instrument join is wanted. See PLAN LIST "optional ISIN-on-tick". |

**Bottom line: the mapping is correct and safe for storing both feeds. No mis-map for Monday.** The only gap is a *future* cross-feed analytics key.

---

## THE PLAN LIST (actionable sequence)

### P0 — operator-supplied (BLOCKER)
1. **Provision Groww SSM creds** — create `/tickvault/prod/groww/api-key` and `/tickvault/prod/groww/totp-secret` (SecureString). No seed script references `groww` today; this is a manual one-time step. Without it the sidecar SSM fetch backs off forever → no Groww ticks.

### P0b — operator decision
2. **Confirm REST-lock scope = market-data-only.** Keep: live-feed AUTH (Dhan `generateAccessToken`/`RenewToken`, Groww `/v1/token`) + static instrument-master CSVs (Dhan Detailed CSV, niftyindices NTM, Groww master) — they build the security_id map + the token the feed needs. Remove: all REST market-data pulls. Captured in `no-rest-except-live-feed-2026-06-27.md`.

### Tonight (Saturday dry-run)
3. **Provision Groww SSM creds** (P0 above — prerequisite for everything below).
4. **Bump `GROWW_MAX_SUBSCRIBE`** to the full universe (~779) so the dry-run exercises the real subscription size (default first-run cap is 60).
5. **Start the instance + enable Groww** — set `feeds.groww_enabled = true` (via `config/base.toml [feeds]` + restart, OR live `POST /api/feeds/groww {enabled:true}` runtime overlay, no restart).
6. **Boot BOTH feeds + verify ZERO errors:**
   - `make doctor` → 7-section all green.
   - `mcp__tickvault-logs__tail_errors` / `list_novel_signatures` → empty.
   - Per-feed QuestDB counts: `select feed, count(*) from ticks where ts > dateadd('h',-1,now()) group by feed` → both `dhan` and `groww` non-zero.
   - Confirm Groww NDJSON streaming (`data/groww/live-ticks.ndjson` growing), watch-list built (`data/groww/groww-watch-<date>.json`), bridge persisting + folding 21 TFs.

### Engineering serial PRs (one-at-a-time per `pr-completion-protocol.md`)
| PR | Scope | State |
|---|---|---|
| **C1 #1216** | converge Dhan+Groww ticks row build into shared `build_tick_row_for_feed` | ✅ **MERGED** (origin/main 60941afe) |
| **C2** | consumer-loop convergence (shared tick consumer across feeds) | next |
| **no-REST-except-live-feed strip** | remove market-data REST: prev-day OHLCV (`/charts/historical`), 1m cross-verify (`/charts/intraday`), `/v2/profile` canary + mid-session watchdog, `/marketfeed/ltp`+`/quote` open-price fallback, `/optionchain`. **KEEP** auth + static master CSVs. | after operator confirms P0b scope |
| **optional ISIN-on-tick** | store ISIN on the `ticks` row to enable cross-feed per-instrument join (closes the mapping caveat) | optional / future |
| **EAP dossier** | external-audit-package dossier | **BLOCKED** on screenshots |

### Monday 2026-06-29
7. **08:30 IST cold boot** — instance auto-starts (weekday schedule); first boot of the day → fail-closed cold universe build, infinite-retry until fresh CSV.
8. **Both feeds subscribe** — Dhan Quote-mode daily universe (100/msg batches, `SubscribeRxGuard` preserves subs across reconnects); Groww watch-list built + sidecar streaming.
9. **09:00 IST live capture** — both feeds flowing into shared `ticks` (`feed` tagged) + 21-TF candles.
10. **09:16 IST self-test** (`SELFTEST-01/02`) — 7 sub-checks green.
11. **Tick-conservation audit** (`TICK-CONSERVE-01`, 15:40 IST) — reconciles WAL vs processed vs persisted per feed.

---

## O(1) HONESTY TABLE

| Operation | Complexity | Proof / note |
|---|---|---|
| Tick header parse (security_id + segment off wire) | **O(1)** | `parse_header` — 4 `from_le_bytes`, `#[inline]`, bounds-checked (`header.rs:22,35-41`) |
| Dhan instrument enrichment lookup | **O(1)** | immutable `HashMap` + `by_composite` `(security_id, exchange_segment)`, no locks/alloc on hot path (`instrument_registry.rs:6-8`) |
| Groww stock id resolution (ISIN→token) | **O(1)** | HashMap built once cold; per-constituent lookup O(1) (`instruments.rs:239-268`) |
| Groww index id (FNV-1a band) | **O(1)** | deterministic, structurally disjoint from stock tokens (`stable_index_security_id`) |
| Tick → persist dedup | **O(1)** | hash UPSERT key `(ts, security_id, segment, capture_seq, feed)` (`tick_persistence.rs:92`) |
| Subscription dispatch batching | **O(N) sub-batched** | `chunks(100)` over the universe (`subscription_builder.rs:63,68`) — inherently proportional to universe size, **flagged O(N), not faked O(1)** |
| **Daily universe build** | **O(N)** | CSV download + parse + dedup + ISIN-join is **inherently O(N) in instrument count — FLAGGED HONESTLY as O(N), NOT claimed O(1).** It is a cold-path, once-per-day build, off the tick hot path. |

---

## Design

Two independent live market-data feeds write to the SAME shared QuestDB tables (`ticks`, `candles_1m..21TF`, audit) distinguished by a zero-alloc `feed` SYMBOL column (`'dhan'`/`'groww'`). Dhan is feed #1 (binary WS off `wss://api-feed.dhan.co`, security_id+segment off the wire). Groww is feed #2 (Python `growwapi` sidecar → NDJSON capture-at-receipt → Rust bridge tails → validates → persists + folds 21 TFs through the SAME `MultiTfAggregator`). The Groww sidecar is auto-launched + supervised by Rust (`groww_sidecar_supervisor.rs`, venv auto-provision, SSM creds, exp-backoff restart, `kill_on_drop(true)`). Each feed has its own connection + pipeline-instance isolation; they share the WAL→ring→spill→DLQ zero-loss chain and the dedup key (with `feed`). Per-feed enable/disable is an O(1) boot-time config read + runtime overlay (`/api/feeds/groww`).

## Edge Cases

- Monday = COLD boot (instance OFF over the weekend), not a sleep-wake — first-boot fail-closed cold universe build, no same-day snapshot.
- 2026-06-26 (Fri) is an NSE holiday; Mon 06-29 is the next trading day (not in `nse_holidays`).
- Groww first-run subscribe cap = 60 unless `GROWW_MAX_SUBSCRIBE` raised → dry-run must raise it to test ~779.
- Groww live feed carries **no volume** (Option A) → Groww candles have zero volume (documented limitation, accepted).
- Numeric id coincidence across feeds → harmless (the `feed` column disambiguates).
- Ambiguous ISIN at Groww mapping → excluded, never guessed (fail-closed); >2% unresolved → degrade-and-alert (`NTM-CONSTITUENCY-01`), core universe still subscribes.
- Trading mode = sandbox until 2026-06-30 → real orders Monday are FORBIDDEN; capture is unaffected.

## Failure Modes

| Mode | Handling |
|---|---|
| Dhan auth timeout / DH-901 | `BootAbortClean` / rotate-retry-once; sleep-wake `force_renewal_if_stale(14400)` (AUTH-GAP-03) |
| QuestDB down at boot | BOOT-01 (30s) → BOOT-02 HALT (60s); rescue ring buffers meanwhile |
| CSV fetch fail | infinite-retry fail-closed, boot BLOCKS — never proceeds on stale/partial data (daily-universe §4) |
| Universe size out of `[100,1200]` | `INSTR-FETCH-04` HALT |
| Reconnect loses subs | `SubscribeRxGuard` re-installs receiver + re-sends cached sub messages |
| QuestDB outage mid-session | ring → NDJSON spill → DLQ; WAL-before-broadcast durable floor |
| Pool task panic | WS-GAP-05 paged; **RISK-1**: supervisor counts but does NOT respawn — single panicked read loop has no in-session self-heal until restart (cold-boot Monday starts fresh, so not a Monday-open blocker) |
| Groww SSM creds missing | sidecar auth fails → backoff loop forever, **no Groww ticks** — the P0 BLOCKER |
| Groww first live run | venv provision + `pip install growwapi` + NATS-WS/protobuf decode never exercised on this host → **dry-run TONIGHT is the mitigation** |
| NTM/Groww master GET fails | Groww degrades (`NTM-CONSTITUENCY-01`), Dhan unaffected |

## Test Plan

- Dhan: existing unit/integration/property/loom/dhat suite is green on `main`; tick parse + dedup + I-P1-11 + SubscribeRxGuard ratchets pinned.
- Groww: pure primitives unit + proptest (mapping, dedup, FNV-band disjointness). **NO live e2e exists** → the Saturday dry-run IS the e2e test (enable off-hours, confirm watch-list builds, sidecar streams NDJSON, bridge persists per-feed, both feeds non-zero in QuestDB, `make doctor` green, `tail_errors` empty).
- Acceptance gate before Monday: dry-run shows ZERO errors + both feeds writing rows + 21 TFs sealing for `feed='groww'`.

## Rollback

- Groww is **default OFF**; flipping `feeds.groww_enabled=false` (config restart OR `POST /api/feeds/groww {enabled:false}`) reverts to byte-identical Dhan-only behaviour. The sidecar is torn down (`kill_on_drop`).
- Dhan-only is the proven baseline; nothing in enabling Groww touches the Dhan pool, tables, or boot path.
- Any engineering PR rolls back via `git revert` of its squash commit (one PR at a time keeps rollback atomic).

## Observability

7-layer per feed: Prom counters/gauges (`tv_websocket_connections_active`, per-feed tick counters), tracing spans, `error!` with `code = ErrorCode::X.code_str()`, Telegram events (edge-triggered, market-hours-gated), `<event>_audit` QuestDB tables, CloudWatch alarms. Monday milestones: 09:14 pre-open readiness, 09:15:30 streaming confirmation, 09:16:30 self-test (`SELFTEST-01/02`), 15:40 tick-conservation audit (`TICK-CONSERVE-01`) per feed. Per-feed QuestDB verification via `mcp__tickvault-logs__questdb_sql` grouped by `feed`.

---

## Honest 100% claim (per operator-charter §F)

> "100% inside the tested envelope, with ratcheted regression coverage: Dhan capture is error-free + O(1)-per-tick + no-mis-map (security_id/segment off the wire) for the Monday cold-boot; both feeds share the ring→spill→DLQ zero-loss chain (`TICK_BUFFER_CAPACITY`, ratcheted by `zero_tick_loss_alert_guard.rs`); the DEDUP key `(ts, security_id, segment, capture_seq, feed)` keeps both feeds' rows distinct (pinned by `test_tick_dedup_key_exact_value`); universe build is honestly O(N) (cold path), NOT claimed O(1). **Groww has NEVER been run live on this host and SSM creds are not provisioned — until the Saturday dry-run passes ZERO-error with both feeds writing rows, Groww readiness is ASSUMED, not Verified.** Cross-feed per-instrument comparison is NOT supported today (ISIN not stored on the tick row)."

## Auto-driver explanation

> Sir, the juice shop now has TWO price boards — one from Dhan, one from Groww — hanging side by side, each with its own ON/OFF switch. Dhan's board has run for weeks and is trusted. Groww's board is brand-new and has never been switched on even once, and its key is still missing from the locker (the SSM creds). So tonight we get the key, switch on Groww quietly after the market closes, and watch both boards fill the register with no red marks. Only if that test is clean do we trust both boards on Monday morning. The register keeps both boards' prices in separate columns so they never overwrite each other.

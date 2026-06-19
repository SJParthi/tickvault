# Pluggable Per-Feed Runtime — High-Level Architecture

> **Authority:** CLAUDE.md > `operator-charter-forever.md` > `groww-second-feed-scope-2026-06-19.md` §32 > this design.
> **Operator demand (2026-06-19, verbatim intent):** *"make everything as common runtime dynamic
> scalable approach — one and only when the feed is ON, only for those feeds it should run."*
> **Status:** DESIGN (design-first wall). Realized incrementally by `.claude/plans/active-plan-groww-second-feed.md` PR-4.
> **Operator approval (2026-06-19, AskUserQuestion):** "Clean Groww-only branch" — the first realization of this design.

---

## 0. The one rule

> **A feed's code runs IF AND ONLY IF its enable flag is `true`. Nothing else.**

`dhan_enabled=true` → Dhan lane runs. `groww_enabled=true` → Groww lane runs. Both → both, in
parallel, isolated. Neither → the process idles (shared infra only), never crashes.

---

## 1. The picture (auto-driver view)

```
              ┌─────────────────────────────────────────────┐
   config →   │            [feeds] on/off switches          │
              │   dhan_enabled=?      groww_enabled=?  ...   │
              └───────────────────────┬─────────────────────┘
                                      │  BOOT DISPATCHER
                                      │  "spawn ONLY the ON lanes"
                ┌─────────────────────┼─────────────────────┐
                │ (if ON)             │ (if ON)             │ (if ON, future)
                ▼                     ▼                     ▼
        ┌───────────────┐    ┌───────────────┐    ┌───────────────┐
        │  DHAN LANE    │    │  GROWW LANE   │    │  FEED #3 LANE │
        │ connect→WAL→  │    │ connect→WAL→  │    │   …same shape │
        │ ring→spill→   │    │ ring→spill→   │    │               │
        │ DLQ→process→  │    │ DLQ→process→  │    │               │
        │ aggregate     │    │ aggregate     │    │               │
        │   ↓           │    │   ↓           │    │               │
        │ ticks /       │    │ groww_live_   │    │ feed3_*       │
        │ candles_*     │    │ ticks /       │    │ tables        │
        │ (Dhan tables) │    │ groww_*       │    │               │
        └───────┬───────┘    └───────┬───────┘    └───────┬───────┘
                └───────────┬────────┴─────────────────────┘
                            ▼   SHARED RUNTIME (started once)
              config · observability · QuestDB · shutdown signal
```

**Each lane is its own sealed pipe.** A lane never reads/writes another lane's tables, never shares a
connection, never shares a pipeline instance. One lane dying cannot touch another.

---

## 2. The four pieces

| Piece | What it is | Where |
|---|---|---|
| **Feed switches** | `FeedsConfig { dhan_enabled, groww_enabled, … }` — per-feed bool, default Dhan-ON / others-OFF | `crates/common/src/config.rs` + `config/base.toml [feeds]` |
| **Boot dispatcher** | reads the switches, spawns ONLY the enabled lanes; if none, idles | `crates/app/src/main.rs` (top of boot) |
| **Feed lane** | self-contained: connect → capture (WAL/ring/spill/DLQ) → process → aggregate → its own `<feed>_*` tables | per-feed module (`feed/dhan/*`, `feed/groww/*`) |
| **Shared runtime** | config, observability, QuestDB, the single shutdown `Arc<Notify>` — built once, used by whichever lanes are on | `crates/app/src/main.rs` (before lane spawn) |

---

## 3. The 4-word test (operator's "common / dynamic / scalable" demand)

| Word | How this design satisfies it |
|---|---|
| **Common** | ONE binary, ONE `docker-compose`, Mac dev = AWS prod. Lanes differ ONLY by a config flag — no `#[cfg]`, no separate build. |
| **Dynamic** | The boot dispatcher reshapes the running system from the flags: today boot-time; the same flag is the seam for a future runtime hot-toggle. |
| **Scalable** | Adding feed #3 = add one flag + one lane module that REUSES the shared WAL→ring→spill→DLQ→aggregator chain. Zero change to existing lanes. O(1) boot cost per disabled lane (it simply isn't spawned). |
| **Incremental** | Every new lane ships **default-OFF**, proven in isolation, flipped on only when ready (Groww is the template). No big-bang. |

---

## 4. Isolation guarantees (why one lane can't hurt another)

| Resource | Per-lane, never shared |
|---|---|
| Connection | Dhan WS ≠ Groww NATS — separate sockets, separate auth |
| Durable capture | each lane has its own WAL segments / ring / spill / DLQ instance |
| Tables | `ticks`/`candles_*` (Dhan) vs `groww_*` (Groww) vs `feed3_*` — DEDUP-segment + the `groww_*`-namespace guard enforce this |
| Pipeline | separate tick-processor + aggregator instances |
| Failure | a lane panic is caught by its own supervisor; the dispatcher + other lanes are untouched |

This is the §32 "Dhan never affected by Groww" guarantee, generalized to N feeds.

---

## 5. Realization path (honest — incremental, not a big-bang rewrite)

The TARGET above is symmetric N lanes. The CODE gets there in steps, so the live Dhan path is never
put at risk:

| Step | What ships | Risk | Status |
|---|---|---|---|
| **A** | `[feeds]` switches + `FeedsConfig` | none | ✅ Done |
| **B** | Groww lane (connect→…→`groww_*`), default-OFF, spawned only if `groww_enabled` | none (isolated) | ✅ Done (sidecar+bridge merged) |
| **C** | **Boot dispatcher gate**: if `!dhan_enabled` → skip the entire Dhan boot block, run shared infra + enabled lanes, idle-await shutdown. (The approved "clean Groww-only branch".) | low — one early branch, Dhan block byte-identical when `dhan_enabled=true` (default) | ⏳ **THIS PR (PR-4)** |
| **D** | Extract today's monolithic Dhan boot into `spawn_dhan_lane()` for full lane symmetry | medium (big refactor of 7.4k-line boot) | 🔵 Future increment |
| **E** | Native-Rust Groww NATS client replaces the Python sidecar (same lane, real socket) | gated on verified wire shape | 🔵 Future |

**Honest envelope:** after Step C, the system already behaves per the one rule (each feed runs iff its
flag is on). Step D is cosmetic symmetry (making the Dhan lane a tidy `spawn_dhan_lane()` like Groww),
NOT a behaviour change — deferred so we never destabilize the live Dhan feed for a refactor.

---

## 6. Step C — the exact gate (this PR)

```rust
// crates/app/src/main.rs — after shared infra + the (already-gated) Groww spawns,
// BEFORE the Dhan fast/slow boot block:
if !feeds.dhan_enabled {
    if feeds.groww_enabled {
        info!("GROWW-ONLY MODE — Dhan boot skipped; Groww lane running, awaiting shutdown");
    } else {
        warn!("NO FEED ENABLED — idle runtime; shared infra only, awaiting shutdown");
    }
    await_shutdown_signal().await;   // tokio::signal::ctrl_c + the shared Notify
    return Ok(());
}
// ── existing Dhan fast/slow boot block runs UNCHANGED when dhan_enabled=true (default) ──
```

- `dhan_enabled=true` (prod default) → **zero behavioural change** (the branch is skipped).
- `dhan_enabled=false, groww_enabled=true` → **true Groww-only** local run.
- both false → idle (no crash) — fail-soft, not fail-hard.

**Ratchets:** `test_feed_dispatcher_dhan_only_default`, `test_groww_only_skips_dhan_boot`,
`test_no_feed_enabled_idles_not_crashes`; source-scan that the Dhan boot block stays behind the gate.

---

## 7. What a PR violating this design looks like (REJECT)

- Runs Dhan code when `dhan_enabled=false` (the bug we're fixing — the old "warn-only" stub).
- A lane reads/writes another lane's tables (Groww writing `ticks`, Dhan reading `groww_*`).
- Adds a feed without a flag, or default-ON for a new feed (must be default-OFF, §incremental).
- A shared resource (one WS, one ring) multiplexed across feeds — breaks isolation.
- "No feed enabled" crashes the process instead of idling.

---

## 8. Auto-driver one-liner

> Sir, the juice shop has two machines — orange (Dhan) and mango (Groww) — each with its own ON/OFF
> switch. The boy turns on ONLY the machines whose switch is ON. Orange on, mango off → only orange
> runs. Both off → the shop is open but quiet, nothing breaks. Want a third machine (apple)? Add a
> switch and a machine — the other two don't change at all. Same shop, same boy, just flip switches.

---

## 9. Trigger / cross-reference

- `crates/common/src/config.rs` (`FeedsConfig`), `config/base.toml [feeds]`
- `crates/app/src/main.rs` (boot dispatcher / lane gate)
- `crates/core/src/feed/groww/*`, future `crates/core/src/feed/dhan/*`
- Plan: `.claude/plans/active-plan-groww-second-feed.md` PR-4
- Lock: `.claude/rules/project/groww-second-feed-scope-2026-06-19.md` §1-§5

# Topic — Top-N Volume Dynamic Depth — LOCKED DESIGN (operator clarified 2026-05-12 morning)

**Created:** 2026-05-12 morning IST
**Trigger:** Operator clarified: NO ATM bootstrap, delta-only swaps, depth-200 MUST cover both NIFTY + BANKNIFTY (specifically: NIFTY CE + NIFTY PE + BANKNIFTY CE + BANKNIFTY PE minimum + 1 wildcard).
**Status:** DESIGN LOCKED. Replaces ATM-bootstrap proposal. Includes full worst-case matrix.

---

## ✅ The locked design (operator-confirmed)

| Rule | Locked |
|---|---|
| ATM bootstrap 09:00-09:15? | ❌ **NO** — depth-20/200 do nothing pre-09:15 |
| Universe filter | `NSE_FNO + OPTIDX` (NIFTY + BANKNIFTY only, excludes SENSEX BSE) |
| Ranking metric | `volume_desc` (pure top volume, NOT change_pct) |
| Window | rolling 60 sec |
| Earliest first swap | ~09:16:00 IST (after first 1-min window) |
| Re-rank interval | every 60 sec |
| Swap mechanism | **delta-only** — unsub only the slots that changed, sub only the new ones |
| Hysteresis | 1.1× new-vs-old (prevents flapping) |
| Depth-20 (250 slots) | **OPEN cohort** — pure top-250 by volume |
| Depth-200 (5 slots) | **FORCED split** — 4 fixed + 1 wildcard (see below) |

---

## 🎯 Depth-200 slot allocation (the locked rule)

Always-present 4 slots + 1 wildcard:

| Slot | Contract type | Selection within type |
|---|---|---|
| **Slot 1** | NIFTY CE | top-volume NIFTY CE across all strikes/expiries |
| **Slot 2** | NIFTY PE | top-volume NIFTY PE across all strikes/expiries |
| **Slot 3** | BANKNIFTY CE | top-volume BANKNIFTY CE across all strikes/expiries |
| **Slot 4** | BANKNIFTY PE | top-volume BANKNIFTY PE across all strikes/expiries |
| **Slot 5** | wildcard | next-highest-volume option across ALL of NIFTY+BANKNIFTY CE/PE, **excluding** anything already in slots 1-4 |

**Why this design:**
- Guarantees coverage of both underlyings AND both sides (calls + puts)
- Slot 5 captures whichever side is hottest after the 4 fixed picks
- Day where NIFTY 22700 CE has 8M volume + NIFTY 22700 PE has 7M → both still get coverage via slots 1+2; BANKNIFTY also covered via slots 3+4

---

## ⏰ Locked timeline (with explicit "do nothing" window)

```
   08:30         09:00              09:15:00.000       09:15:30           09:16:00
   AWS boot      WS pool wakes      F&O OPENS          partial 30s data    1-MIN DATA READY
                                    first option tick
       │             │                    │                   │                   │
       ▼             ▼                    ▼                   ▼                   ▼
   ┌─────────┬─────────────────┬─────────────────┬──────────────────┬─────────────────┐
   │ Boot    │ WS conns OPEN   │ F&O ticks START │ Volume window    │ FIRST RANKING   │
   │         │                 │ flowing         │ accumulating     │ → FIRST SWAP    │
   │         │ Main feed:      │                 │                  │                 │
   │         │ subscribe to    │ Depth-20/200    │ Depth-20/200     │ Subscribe top   │
   │         │ ~11K            │ STILL IDLE      │ STILL IDLE       │ 250 + 5         │
   │         │ instruments     │ (no top-N data  │ (waiting for     │                 │
   │         │                 │ yet)            │ 1-min window)    │ Re-rank every   │
   │         │ Depth-20/200    │                 │                  │ 60 sec from now │
   │         │ conns OPEN but  │                 │                  │                 │
   │         │ ZERO subs       │                 │                  │ DELTA swaps     │
   │         │                 │                 │                  │ only            │
   └─────────┴─────────────────┴─────────────────┴──────────────────┴─────────────────┘
   
   Idle window for depth-20/200: 09:00 → 09:16 (16 min — but only 1 min of that
   is "missing data"; the rest is "F&O market closed anyway")
```

---

## 🔧 The delta-only swap algorithm

**Goal:** if old top-5 = `[A, B, C, D, E]` and new top-5 = `[A, B, C, D, F]`, only do:
- UNSUB E (one frame)
- SUB F (one frame)

NOT: unsub all 5 + sub all 5 (would be 10 frames + brief gap on slots 1-4).

### Algorithm

```rust
fn compute_delta(old: &[SecurityId; 5], new: &[SecurityId; 5])
    -> (Vec<SecurityId>, Vec<SecurityId>) // (to_unsub, to_sub)
{
    let old_set: HashSet<_> = old.iter().collect();
    let new_set: HashSet<_> = new.iter().collect();
    
    let to_unsub: Vec<_> = old.iter().filter(|x| !new_set.contains(x)).cloned().collect();
    let to_sub:   Vec<_> = new.iter().filter(|x| !old_set.contains(x)).cloned().collect();
    
    (to_unsub, to_sub)
}
```

Apply per-connection: each of the 5 depth-200 conns has 1 slot. Find which conn holds the to_unsub SID, send unsub frame on THAT conn, then sub frame for the to_sub SID.

For depth-20 (250 slots across 5 conns): same algorithm at 250-element scale. Slots assigned to specific conns (e.g., slot 0-49 = conn 1, 50-99 = conn 2 ...). Delta diff figures out which conns need which (unsub, sub) pairs.

**Existing code:** `DepthCommand::Swap20 { unsub_ids, sub_ids }` and `DepthCommand::Swap200 { unsub_id, sub_id }` already designed for this — just need to verify the selector emits diff-only commands, not blanket "re-subscribe everything."

---

## 📊 Depth-200 worked example (Tuesday 09:16:00)

**Movers_1m top by volume (last 60s):**

| Rank | SID | Symbol | Strike | Type | Underlying | Volume |
|---|---|---|---|---|---|---|
| 1 | 41742 | NIFTY-22700-CE-15May | 22700 | CE | NIFTY | 8.4M |
| 2 | 41743 | NIFTY-22700-PE-15May | 22700 | PE | NIFTY | 7.9M |
| 3 | 39021 | BANKNIFTY-48200-CE-15May | 48200 | CE | BANKNIFTY | 6.1M |
| 4 | 39022 | BANKNIFTY-48200-PE-15May | 48200 | PE | BANKNIFTY | 5.8M |
| 5 | 41744 | NIFTY-22800-CE-15May | 22800 | CE | NIFTY | 4.2M |
| 6 | 41745 | NIFTY-22600-PE-15May | 22600 | PE | NIFTY | 3.8M |
| 7 | 41746 | NIFTY-22650-CE-15May | 22650 | CE | NIFTY | 3.5M |

**Slot allocation per the rule:**

| Slot | Filter | Winner |
|---|---|---|
| 1 (NIFTY CE) | top NIFTY CE | 41742 (NIFTY-22700-CE, 8.4M) |
| 2 (NIFTY PE) | top NIFTY PE | 41743 (NIFTY-22700-PE, 7.9M) |
| 3 (BANKNIFTY CE) | top BANKNIFTY CE | 39021 (BANKNIFTY-48200-CE, 6.1M) |
| 4 (BANKNIFTY PE) | top BANKNIFTY PE | 39022 (BANKNIFTY-48200-PE, 5.8M) |
| 5 (wildcard) | top remaining (not in 1-4) | 41744 (NIFTY-22800-CE, 4.2M) ← could have been any |

**Result:** all 5 depth-200 conns subscribed. NIFTY + BANKNIFTY both covered. ✅

---

## 📊 Depth-200 worked example with re-rank at 09:17:00

**Imagine BANKNIFTY 48200 PE volume drops, BANKNIFTY 48300 PE surges:**

| Rank | SID | Symbol | Volume |
|---|---|---|---|
| 1 | 41742 | NIFTY-22700-CE | 9.2M (still #1) |
| 2 | 41743 | NIFTY-22700-PE | 8.5M (still #2) |
| 3 | 39021 | BANKNIFTY-48200-CE | 6.4M (still top BNF CE) |
| 4 | **39025** | **BANKNIFTY-48300-PE** | **6.0M** (new top BNF PE) |
| 5 | 39022 | BANKNIFTY-48200-PE | 5.9M (was slot 4, now wildcard maybe) |

**New slot allocation:**

| Slot | Was | Now | Change |
|---|---|---|---|
| 1 | 41742 | 41742 | no change |
| 2 | 41743 | 41743 | no change |
| 3 | 39021 | 39021 | no change |
| 4 | 39022 | **39025** | ✅ CHANGED |
| 5 | 41744 | **39022** (wildcard fell to here) | ✅ CHANGED |

**Delta commands:**
- Conn 4: unsub 39022, sub 39025
- Conn 5: unsub 41744, sub 39022

NOT unsub all 5 + sub all 5.

---

## 📱 Telegram message on first swap (09:16:00)

```
🎯 DEPTH ARMED — top volume contracts subscribed
Time: 09:16:00 IST · 1-minute window complete

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
📊 DEPTH-200 (forced split)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  Slot 1 (NIFTY CE):     NIFTY 22700 CE · 8.4M
  Slot 2 (NIFTY PE):     NIFTY 22700 PE · 7.9M
  Slot 3 (BANKNIFTY CE): BANKNIFTY 48200 CE · 6.1M
  Slot 4 (BANKNIFTY PE): BANKNIFTY 48200 PE · 5.8M
  Slot 5 (wildcard):     NIFTY 22800 CE · 4.2M

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
📊 DEPTH-20 (open cohort, top 250 by volume)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  NIFTY options:       162 contracts
  BANKNIFTY options:    88 contracts
  Total:               250 / 250 ✅
  
  Volume range: 0.1M → 8.4M

━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🔄 Re-ranking every 60 sec (delta-only swaps)
   Next check: 09:17:00 IST
```

## 📱 Telegram on subsequent re-rank (only if change)

If 0 SIDs changed → SILENT (no message). Only post message if at least 1 swap happened:

```
🔄 DEPTH RE-RANKED at 09:17:00

Depth-200 changes:
  Slot 4: BANKNIFTY 48200 PE → BANKNIFTY 48300 PE (volume shift)
  Slot 5: NIFTY 22800 CE → BANKNIFTY 48200 PE (wildcard rotated)

Depth-20 changes:
  3 contracts swapped out, 3 swapped in (out of 250)

Hysteresis: 1.1× threshold satisfied
```

---

## 🚨 Extreme worst-case scenario matrix (the 20 things that can break)

| # | Scenario | What goes wrong | How we handle | Telegram tier |
|---|---|---|---|---|
| **WC1** | First 60s window has 0 NIFTY CE traded | Slot 1 has no candidate | Hold slot 1 EMPTY (no subscription) + alert | 🟡 WARNING |
| **WC2** | First 60s window has 0 BANKNIFTY traded (any side) | Slots 3+4 empty | Hold empty + alert; depth-200 partial | 🟡 WARNING |
| **WC3** | All 4 fixed slots filled, but no 5th candidate (only 4 distinct SIDs across NIFTY+BANKNIFTY all sides) | Slot 5 has no wildcard | Hold slot 5 empty + alert | 🟡 WARNING |
| **WC4** | Same SID appears as top NIFTY CE AND top wildcard | Dedup conflict | Skip wildcard if SID already in slots 1-4; fall through to next candidate | (silent — handled) |
| **WC5** | movers_1m table has 0 rows (mat view broken) | Selector returns empty set | Don't fire swap; keep last-known assignments; alert | 🔴 CRITICAL (`DEPTH-DYN-04` NEW) |
| **WC6** | movers_1m is stale (last refresh > 5 min ago) | Stale data used as if fresh | Detect timestamp gap; alert; hold last-known | 🟡 WARNING (`DEPTH-DYN-05` NEW) |
| **WC7** | QuestDB query times out (5s timeout) | Selector can't fetch cohort | Retry 3× then skip this minute's re-rank | 🟡 WARNING |
| **WC8** | Swap20 command channel broken (existing DEPTH-20-DYN-02) | Conn lost mid-day | Pool supervisor respawns within 5s | 🚨 EMERGENCY (existing handler) |
| **WC9** | Swap200 command channel broken | Same as WC8 | Same | 🚨 EMERGENCY |
| **WC10** | Dhan rejects subscribe ACK | New SID not actually subscribed | Subscribe-ACK parser (Item 0.5.16) catches; retry | 🟡 WARNING |
| **WC11** | New top-N has SID for contract that just expired (Thu 15:30) | Subscribe to dead contract | Universe filter excludes expired before ranking; if slipped → Dhan rejects → WC10 | (caught upstream) |
| **WC12** | Hysteresis race: vol_new=1.0998× vol_old (just below 1.1) | No swap even though new is higher | By design — operator chose stability over reactivity. Next minute may swap. | (silent) |
| **WC13** | Universe rebuild mid-day adds new contracts | Selector sees them in next minute's cohort | normal — new contracts compete on volume | (silent) |
| **WC14** | Volume monotonicity breach (VOLUME-MONO-01 fires) | Tick data corrupted? | Existing guard fires; rank still uses last good values | 🟡 WARNING (existing) |
| **WC15** | Market halt mid-session (NSE circuit) | Volume freezes mid-rank | Detect ticks_per_sec ≈ 0; freeze re-rank until ticks resume | 🟡 WARNING |
| **WC16** | NSE bhavcopy says contract A had volume X, our movers_1m says Y | Cross-verify at 16:00 fails | Cross-verify alert; investigate next day | 🟡 WARNING |
| **WC17** | Holiday tomorrow → today is the day before; volume in nearest-expiry surges | Top-N heavily expiry-weighted | normal — reflects reality of expiry-day positioning | (silent) |
| **WC18** | First-tick of session at 09:15:00.000 has volume=12000 (not 0) — Dhan didn't reset | VOLUME-MONO-01 catches | Existing guard | 🟡 WARNING |
| **WC19** | All 250 depth-20 slots want the SAME single hot contract (degenerate) | Cohort has only 1 distinct SID | Dedup → 1 sub + 249 slots empty + alert | 🔴 CRITICAL (`DEPTH-DYN-06` NEW) |
| **WC20** | Re-rank computation itself takes > 60s (slow QuestDB) | Tick re-rank cycle blocked | Spawn new task; never queue them; skip if previous still running | (silent — handled) |

---

## 🛡 Cold-start specific handling (09:15:00 → 09:16:00)

| Sub-window | Behavior |
|---|---|
| **00:00 → 09:14:59** | Depth-20/200 conns idle (no F&O market). NO subscription frames. |
| **09:15:00 → 09:15:59** | First option ticks flowing. `movers_1m` row not yet emitted (mat view samples every 60s). Selector NOT invoked. Conns still idle. |
| **09:16:00.000** | First `movers_1m` row produced. Selector runs. Top-5 + top-250 computed. Subscribe frames fire on each conn. |
| **09:16:00.500** | Subscribe-ACK parser confirms Dhan accepted. First depth packets start flowing. |
| **09:16:30** | First Telegram `DepthArmed` ping fires (after subscribe ACK + first packet confirmation). |
| **09:17:00** | Second re-rank. Delta diff vs 09:16 set. Only swap what changed. |

---

## 🔄 Updated config

```toml
[depth_20.dynamic]
conns = 5
sids_per_conn = 50

[depth_20.dynamic.universe]
exchange_segments = ["NSE_FNO"]
instrument_types = ["OPTIDX"]
min_liquidity_volume = 0
rerank_metric = "volume_desc"          # operator-locked
window_secs = 60
hysteresis_factor = 1.1                # NEW — added per operator design
cold_start_mode = "idle"               # NEW — no ATM bootstrap
delta_only_swaps = true                # NEW — only diff swaps

[depth_200.dynamic]
conns = 5
sids_per_conn = 1

[depth_200.dynamic.universe]
exchange_segments = ["NSE_FNO"]
instrument_types = ["OPTIDX"]
min_liquidity_volume = 0
rerank_metric = "volume_desc"
window_secs = 60
hysteresis_factor = 1.1
cold_start_mode = "idle"
delta_only_swaps = true

# NEW — depth-200 forced split rule
[depth_200.dynamic.slot_allocation]
mode = "forced_split"
slot_1 = { underlying = "NIFTY", option_type = "CE" }
slot_2 = { underlying = "NIFTY", option_type = "PE" }
slot_3 = { underlying = "BANKNIFTY", option_type = "CE" }
slot_4 = { underlying = "BANKNIFTY", option_type = "PE" }
slot_5 = { mode = "wildcard" }
```

---

## 🛠 Friday Phase 0.5 task — updated LoC estimate

| # | Sub-task | LoC |
|---|---|---|
| 1 | Verify `depth_dynamic_pipeline_v2::spawn` wired at main.rs boot | 20 |
| 2 | Flip `rerank_metric` config + drop `min_liquidity_volume` floor | 10 |
| 3 | Add 1.1× hysteresis rule to selector | 50 |
| 4 | **Implement depth-200 forced-split slot allocator** (NEW) | 150 |
| 5 | **Delta-only swap diff algorithm** (verify or implement) | 80 |
| 6 | Add `DepthArmed` notification (first swap) + `DepthReRanked` (subsequent if delta>0) | 100 |
| 7 | Add 3 NEW ErrorCodes: DEPTH-DYN-04 (empty cohort), DEPTH-DYN-05 (stale mat view), DEPTH-DYN-06 (degenerate single-SID cohort) | 60 |
| 8 | Ratchet test: capacity 5×50=250 + 5×1=5 + 4-of-5-forced-split + delta-only | 80 |
| 9 | Cold-start state machine: idle pre-09:15 → wait 09:15-09:16 → first swap 09:16:00 | 80 |
| 10 | 20-scenario worst-case chaos test set (WC1-WC20) | 250 |

**Total: ~880 LoC.** Friday Session 6 (raised from 200 LoC initial estimate).

---

## 📋 Phase 0.5 grand total update

Adding this fully-specced item:

| | Count | LoC |
|---|---|---|
| Mon eve Phase 0.5 (locked) | 23 items | 3,330 |
| **+ Dynamic Top-N Depth item (this turn)** | +1 item | **+880** |
| **NEW grand total** | **24 items** | **~4,210 LoC** |

Friday distribution: 6 parallel sessions × ~700 LoC each.

---

## 🚖 Auto-driver / Insta-reel summary

> "Sir, locked in:
>
> **NO ATM bootstrap.** Depth-20/200 conns sit idle from 09:00 to 09:16. First swap at 09:16:00 sharp.
>
> **Depth-20 (250 slots)** = open cohort, top 250 by volume across NIFTY + BANKNIFTY options. No forced quotas.
>
> **Depth-200 (5 slots)** = forced split. Always 1 each of: NIFTY CE / NIFTY PE / BANKNIFTY CE / BANKNIFTY PE. The 5th slot is wildcard — next highest after the 4 fixed picks.
>
> **Re-rank every 60 sec.** DELTA SWAPS ONLY — if slots 1-4 didn't change and only slot 5 swapped, just 1 unsub + 1 sub frame. Not blanket unsub-all + sub-all.
>
> **20 worst-case scenarios** mapped: empty cohort, dedup conflict, slow QuestDB, stale mat view, expiry rollover, NSE circuit halt, etc. Each has detection + handling + Telegram tier.
>
> **Friday task ~880 LoC** in Session 6."

---

## 🎤 Confirm or adjust?

| Pick | Means |
|---|---|
| **A** | "LOCKED — proceed with this design for Friday" |
| **B** | "Change slot allocation rule — show me X variant" |
| **C** | "More worst-cases I want covered (tell me what to add)" |
| **D** | "Different angle — drop next topic" |

Floor's yours bro. ☕

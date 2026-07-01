# Claude Code Projects (Beta) — Round-4 Feedback

**From:** Parthiban — Max plan, macOS desktop app
**Date:** 2026-07-01
**Topic:** Project-navigation regression for parallel / multi-project workflows

## Summary

Overnight the left-sidebar project navigation changed from an **auto-populated "Recents"** list to a **manual "Pinned"-only** list. Projects no longer surface on their own; each project must be manually pinned to appear in the sidebar; the project view opens as a cramped narrow side panel that must be expanded; and none of that state persists when switching between projects. For a workflow that runs 15–20 projects in parallel — I deliberately opened many for stress-testing — this makes it hard to tell which project I was working in, which needs focus, and which to pin, forcing me to open every project one-by-one just to check where the work is.

## Before vs Now

| # | What you do | Before (worked) | Now (broken / confusing) |
|---|---|---|---|
| 1 | Look at the left sidebar | Auto **"Recents"** list surfaced active sessions by itself (Image 1–2) | Manual **"Pinned"-only** list — active work never appears on its own (Image 3–8) |
| 2 | Create / open a project | Auto-appears in the sidebar by name, always visible | Stays buried in the Projects grid — sidebar shows 4 of ~18 (Image 3) |
| 3 | Know which project you're in | Active / recent project was obvious | No focus signal — with 15–20 parallel projects you lose your place |
| 4 | Find where the work is | State was visible at a glance | Must open each project one-by-one to see "needs input" / "ready for review" (Image 4 & 6) |
| 5 | Decide what to pin | Nothing to pin — it was automatic | Pinning is guesswork — nothing tells you which project matters |
| 6 | Open a project | Full view | Cramped narrow side panel, must expand every time (Image 6 & 8) |
| 7 | Switch project → project | State persisted | Pin/expand resets — repeat the whole dance each switch (Image 4→5) |
| 8 | Run many projects in parallel | Zero-click navigation | The workflow this change punishes most — lost context + repetitive manual steps |

## Net impact

What used to be zero-click persistent navigation is now repeated manual pin+expand on every context switch — exactly the friction hit when opening 15–20 parallel projects for stress-testing. The core ask: bring back an auto-populated recent/active-project list in the sidebar (or make the active project persistent), and let the project view stay expanded across switches.

## Evidence — 8 screenshots, in sequence

### Before (the good behavior — evening, ~10:47 PM)

**Image 1** — "Engineering standards review" session. Left sidebar shows an auto-populated **Recents** list (Tick vault dhan skills, Tickvault AWS, Brutex, Engineering standards review). Active work surfaces on its own.

![Image 1](images/round4/image-1.png)

**Image 2** — "Tickvault AWS" session, same auto **Recents** sidebar. Proof the good behavior existed hours before the change.

![Image 2](images/round4/image-2.png)

### After (the regression — next morning, ~7:56–7:57 AM)

**Image 3** — Projects grid shows ~18 projects, but the left sidebar is now **Pinned-only** and lists just 4 — none auto-matching the grid; the "Sessions you start will show up here" empty state persists.

![Image 3](images/round4/image-3.png)

**Image 4** — Inside the TickVault project: "Pinned 0 / No sessions are pinned," and TickVault itself is not in the sidebar even though I am inside it.

![Image 4](images/round4/image-4.png)

**Image 5** — Same view after clicking the pin — TickVault now appears in the sidebar. This proves you must manually pin each project to make it visible.

![Image 5](images/round4/image-5.png)

**Image 6** — The "Tickvalut aws" project opens as a cramped narrow side thread panel that must be manually expanded.

![Image 6](images/round4/image-6.png)

**Image 7** — The "Tick vault dhan skills" session thread, for reference.

![Image 7](images/round4/image-7.png)

**Image 8** — TickVault project with the Thread panel side-by-side; "Pinned 0," and the pin/expand state does not persist across switches.

![Image 8](images/round4/image-8.png)

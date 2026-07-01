# Claude Code Projects (Beta) — Round-4 Feedback

**From:** Parthiban — Max plan, macOS desktop app
**Date:** 2026-07-01
**Topic:** Project-navigation regression for parallel / multi-project workflows

## Summary

Overnight the left-sidebar project navigation changed from an **auto-populated "Recents"** list to a **manual "Pinned"-only** list. Projects no longer surface on their own; each project must be manually pinned to appear in the sidebar; opening a session inside a project shows a cramped narrow side panel that must be expanded; and none of that state persists when switching between projects. For a workflow that runs 15–20 projects in parallel — I deliberately opened many for stress-testing — this makes it hard to tell which project I was working in, which needs focus, and which to pin, forcing me to open every project one-by-one just to check where the work is.

> Note: the projects visible in the sidebar in Images 3–8 are ones I manually pinned for this demo — nothing lands in the sidebar on its own anymore.

## Before vs Now

| # | What you do | Before (worked) | Now (broken / confusing) |
|---|---|---|---|
| 1 | Look at the left sidebar | Auto **"Recents"** list surfaced active sessions by itself (Image 1–2) | Manual **"Pinned"-only** list — only projects you manually pin appear; active work never surfaces on its own (Image 3–8) |
| 2 | Create / open a project | Auto-appears in the sidebar by name, always visible | Stays buried in the Projects grid — sidebar shows 4 of ~18 (Image 3) |
| 3 | Know which project you're in | Active / recent project was obvious | No focus signal — with 15–20 parallel projects you lose your place |
| 4 | Find where the work is | State was visible at a glance | Must open each project one-by-one to see "needs input" / "ready for review" (Image 4 & 6) |
| 5 | Decide what to pin | Nothing to pin — it was automatic | Pinning is guesswork — nothing tells you which project matters |
| 6 | Open a session inside a project | Full view | Opens as a cramped narrow right-side panel you must manually expand (Image 6 & 8) |
| 7 | Switch project → project | State persisted | Pin/expand resets — you repeat the whole dance on each switch |
| 8 | Run many projects in parallel | Zero-click navigation | The workflow this change punishes most — lost context + repetitive manual steps |

## Net impact

What used to be zero-click persistent navigation is now repeated manual pin+expand on every context switch — exactly the friction hit when opening 15–20 parallel projects for stress-testing. The core ask: bring back an auto-populated recent/active-project list in the sidebar (or make the active project persistent), and keep the session panel expanded across switches.

## Evidence — 8 screenshots, in sequence

### Before (the good behavior — Jun 30, ~10:47 PM IST)

**Image 1** (Jun 30, ~10:47 PM IST) — "Engineering standards review" session. Left sidebar shows an auto-populated **Recents** list (Tick vault dhan skills, Tickvault AWS, Brutex, Engineering standards review). Active work surfaces on its own.

**Image 2** (Jun 30, ~10:47 PM IST) — "Tickvault AWS" session, same auto **Recents** sidebar. Proof the good behavior existed hours before the change.

### After (the regression — Jul 1, ~7:56–7:57 AM IST)

**Image 3** (Jul 1, ~7:56 AM IST) — Projects grid shows ~18 projects, but the left sidebar is now **Pinned-only** and lists just 4 (the ones I manually pinned); the "Sessions you start will show up here" empty state persists.

**Image 4** (Jul 1, ~7:57 AM IST) — Inside the TickVault project: "Pinned 0 / No sessions are pinned," and TickVault itself is not in the sidebar even though I am inside it.

**Image 5** (Jul 1, ~7:57 AM IST) — Same view after clicking the pin — TickVault now appears in the sidebar. This proves you must manually pin each project to make it visible.

**Image 6** (Jul 1, ~7:57 AM IST) — The "Tickvalut aws" project is open full-width; opening a session shows its thread as a cramped narrow panel on the right that must be expanded.

**Image 7** (Jul 1, ~7:57 AM IST) — A session thread inside a project, shown for context (the "Tick vault dhan skills" session).

**Image 8** (Jul 1, ~7:57 AM IST) — TickVault with a session thread open as a narrow right-side panel; in practice this panel opens cramped and its expanded state doesn't carry over when switching projects.

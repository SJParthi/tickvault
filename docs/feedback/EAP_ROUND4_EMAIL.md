Hi Tim,

One more from the beta — a UX regression this time, not infra.

Overnight the Projects left-sidebar changed from an auto-populated "Recents" list to a manual "Pinned"-only list. Projects no longer surface on their own; each one must be manually pinned to appear; opening a session inside a project shows a cramped narrow panel I have to expand; and none of that state persists when I switch between projects. Running 15–20 projects in parallel (I open many deliberately for stress-testing), it's now hard to tell which project I was in, which needs focus, and which to pin — I end up opening every project one-by-one just to find where the work is.

(Note: the projects visible in the sidebar in Images 3–8 are ones I manually pinned for this demo — nothing lands there on its own anymore.)

Before vs Now:

| # | What you do | Before (worked) | Now (broken / confusing) |
|---|---|---|---|
| 1 | Look at the left sidebar | Auto "Recents" list surfaced active sessions by itself (Image 1–2) | Manual "Pinned"-only list — only projects you manually pin appear; active work never surfaces on its own (Image 3–8) |
| 2 | Create / open a project | Auto-appears in the sidebar by name, always visible | Stays buried in the Projects grid — sidebar shows 4 of ~18 (Image 3) |
| 3 | Know which project you're in | Active / recent project was obvious | No focus signal — with 15–20 parallel projects you lose your place |
| 4 | Find where the work is | State was visible at a glance | Must open each project one-by-one to see "needs input" / "ready for review" (Image 4 & 6) |
| 5 | Decide what to pin | Nothing to pin — it was automatic | Pinning is guesswork — nothing tells you which project matters |
| 6 | Open a session inside a project | Full view | Opens as a cramped narrow right-side panel you must manually expand (Image 6 & 8) |
| 7 | Switch project → project | State persisted | Pin/expand resets — you repeat the whole dance on each switch |
| 8 | Run many projects in parallel | Zero-click navigation | The workflow this change punishes most — lost context + repetitive manual steps |

Net impact: what used to be zero-click persistent navigation is now repeated manual pin+expand on every context switch — exactly the friction when running 15–20 parallel projects for stress-testing. The ask: bring back an auto-populated recent/active-project list in the sidebar (or make the active project persistent), and keep the session panel expanded across switches.

Screenshots attached, in order (all times IST):
- Image 1 (Jun 30, ~10:47 PM IST) — before: "Engineering standards review" session; sidebar shows the auto "Recents" list.
- Image 2 (Jun 30, ~10:47 PM IST) — before: "Tickvault AWS" session; same auto "Recents" sidebar. Proves the good behavior existed hours earlier.
- Image 3 (Jul 1, ~7:56 AM IST) — after: Projects grid shows ~18 projects, but the sidebar is now "Pinned"-only and lists just 4 (the ones I manually pinned).
- Image 4 (Jul 1, ~7:57 AM IST) — after: inside the TickVault project, "Pinned 0", and TickVault itself is not in the sidebar.
- Image 5 (Jul 1, ~7:57 AM IST) — after: same view after clicking the pin — TickVault now appears (proves you must manually pin each project).
- Image 6 (Jul 1, ~7:57 AM IST) — after: the "Tickvalut aws" project is open full-width; opening a session shows its thread as a cramped narrow panel on the right that must be expanded.
- Image 7 (Jul 1, ~7:57 AM IST) — after: a session thread inside a project, shown for context (the "Tick vault dhan skills" session).
- Image 8 (Jul 1, ~7:57 AM IST) — after: TickVault with a session thread open as a narrow right-side panel; in practice this panel opens cramped and its expanded state doesn't carry over when switching projects.

Thanks,
Parthi

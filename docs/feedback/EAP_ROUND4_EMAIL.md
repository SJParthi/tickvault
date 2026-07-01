Hi Tim,

One more from the beta — a UX regression this time, not infra.

Overnight the Projects left-sidebar changed from an auto-populated "Recents" list to a manual "Pinned"-only list. Projects no longer surface on their own; each one must be manually pinned to appear; the project opens as a cramped narrow panel I have to expand; and none of that state persists when I switch between projects. Running 15–20 projects in parallel (I open many deliberately for stress-testing), it's now hard to tell which project I was in, which needs focus, and which to pin — I end up opening every project one-by-one just to find where the work is.

Before vs Now:

| # | What you do | Before (worked) | Now (broken / confusing) |
|---|---|---|---|
| 1 | Look at the left sidebar | Auto "Recents" list surfaced active sessions by itself (Image 1–2) | Manual "Pinned"-only list — active work never appears on its own (Image 3–8) |
| 2 | Create / open a project | Auto-appears in the sidebar by name, always visible | Stays buried in the Projects grid — sidebar shows 4 of ~18 (Image 3) |
| 3 | Know which project you're in | Active / recent project was obvious | No focus signal — with 15–20 parallel projects you lose your place |
| 4 | Find where the work is | State was visible at a glance | Must open each project one-by-one to see "needs input" / "ready for review" (Image 4 & 6) |
| 5 | Decide what to pin | Nothing to pin — it was automatic | Pinning is guesswork — nothing tells you which project matters |
| 6 | Open a project | Full view | Cramped narrow side panel, must expand every time (Image 6 & 8) |
| 7 | Switch project → project | State persisted | Pin/expand resets — repeat the whole dance each switch (Image 4→5) |
| 8 | Run many projects in parallel | Zero-click navigation | The workflow this change punishes most — lost context + repetitive manual steps |

Net impact: what used to be zero-click persistent navigation is now repeated manual pin+expand on every context switch — exactly the friction when running 15–20 parallel projects for stress-testing. The ask: bring back an auto-populated recent/active-project list in the sidebar (or make the active project persistent), and keep the project view expanded across switches.

Screenshots attached, in order:
- Image 1 (evening) — before: "Engineering standards review" session; sidebar shows the auto "Recents" list.
- Image 2 (evening) — before: "Tickvault AWS" session; same auto "Recents" sidebar. Proves the good behavior existed hours earlier.
- Image 3 (next morning) — after: Projects grid shows ~18 projects, but the sidebar is now "Pinned"-only and lists just 4.
- Image 4 — after: inside the TickVault project, "Pinned 0", and TickVault itself is not in the sidebar.
- Image 5 — after: same view after clicking the pin — TickVault now appears (proves you must manually pin each project).
- Image 6 — after: "Tickvalut aws" opens as a cramped narrow side panel that must be expanded.
- Image 7 — after: the "Tick vault dhan skills" session thread, for reference.
- Image 8 — after: TickVault with the thread panel side-by-side; pin/expand state does not persist across switches.

Thanks,
Parthi

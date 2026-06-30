# Claude Code Projects (beta) — Round-3 Feedback

> Detailed observations, requests, and open questions accompanying the round-3 feedback email. Prepared 2026-06-30. Where we could not prove a cause, we have said so plainly.

---

## Issues at a glance

| # | Observation |
|---|-------------|
| 1 | Transient "Server is temporarily limiting requests (not your usage limit) · Rate limited" errors recur and terminate sub-agents mid-task; the agent dies with no automatic back-off or resume, so its in-flight work is lost until it is manually re-spawned. |
| 2 | It appears specifically when **more than one Project is open in parallel on the same repository** (roughly 1→5 on one repo, 1–2 on another). With a single Project we did not see it the same way. |
| 3 | **By contrast, normal non-Project sessions never hit it** — we have run as many as 10 parallel normal sessions with no rate-limiting at any point, which is why this looks specific to the Projects beta rather than to concurrency itself. |
| 4 | The message says "not your usage limit," yet the beta was kindly granted elevated access — so from our side it is unclear which limit is being reached. |
| 5 | Sessions under the Projects beta felt noticeably **slower** than a normal, non-Project session — observed many times, not once. |
| 6 | *(Still present from round 2, re-flagged briefly:)* sub-agents stay "running" indefinitely with no auto-termination — they keep spinning and accumulate, and may compound the load behind items 1 and 5. |

---

## What we would love (raised gently)

- **Guidance on the limit** — a one-line clarification of what class of limit the transient throttle represents under the Projects beta, and whether the elevated beta access covers parallel sub-agent and multi-Project throughput or primarily the main session.
- **Confirmation on Projects-per-repo** — whether opening more than one Project on the same repository is supported / advisable, and if so a safe ceiling (Projects-per-repo and parallel-agent count) we can stay within.
- **Automatic back-off and resume** on a transient, server-side ("not your usage limit") throttle, so a sub-agent waits and retries rather than dying and losing its work.
- **A look at the perceived slowness** under Projects versus a normal session, and what keeps it responsive at this scale.
- **A clearer error distinction** between an account usage-limit and a temporary server-side throttle, so a user knows whether to wait or to stop.
- *(Re-raised from round 2:)* idle-agent auto-termination and cleanup, so never-terminated agents stop accumulating.

---

## Open questions

- We have run up to 10 parallel normal (non-Project) sessions without ever hitting this throttle — so why would two or more Projects on a single repository trigger it when 10 parallel normal sessions do not? That contrast is what makes us think it is specific to the Projects layer rather than to concurrency.
- Is opening more than one Project on the same repository the trigger — e.g. do multiple Projects on one repo multiply the effective concurrency against a shared limit? If so, what Projects-per-repo and parallel-agent count would you recommend?
- Does the elevated beta access apply to sub-agent and parallel throughput, or primarily to the main loop?
- Could the signals be linked — for example, sub-agents that never terminate holding concurrency slots, which in turn drives both the slowness and the rate-limiting? We raise this only as a hypothesis, not a claim.
- Is "Server is temporarily limiting requests (not your usage limit)" an expected, self-clearing condition we should simply back off on — or a signal that we are doing something we should not?

---

## Details

### 1 — Transient rate-limiting terminates sub-agents, with no resume

During Projects-beta sessions we repeatedly saw sub-agents terminate with the exact message below — in our most recent session it fired twice in a row, back to back. In several cases the failing task was light (a single web fetch, or a short read-only status check), so it did not appear to be a heavy-token operation that tripped it. Retrying after a short wait usually succeeded, which suggests a transient, self-clearing throttle rather than an exhausted account limit. The agent does not wait and retry on its own — it dies, and its in-flight work is lost until the orchestrator notices and re-spawns it.

```
API Error: Server is temporarily limiting requests (not your usage limit) · Rate limited
```

**Why it matters:** in an unattended multi-agent run, a sub-agent that dies on a transient throttle silently loses its work. A built-in wait-and-retry (back-off and resume) would keep the batch moving on its own.

### 2 — The trigger: more than one Project on the same repository

The clearest pattern we can point to is the number of Projects open on a single repository. With one Project on a repo, we did not see this the same way. Once we had two or more Projects open in parallel against the **same repository** (we worked up from roughly 1 to 5 on one repo, and ran 1–2 on another), each potentially spawning parallel review and worker sub-agents, the transient rate-limiting and the slowness recurred. We cannot isolate with certainty whether it is the multiple-Projects-per-repo setup, the parallel-agent concurrency, or the overall scale — but the "more than one Project on the same repo" condition is the consistent trigger we observed.

**Why it matters:** if multiple Projects on one repository multiply the effective concurrency against a shared limit, knowing that would let us consolidate to a single Project per repo and avoid the throttle entirely.

### 3 — The contrast: normal sessions never throttle, even 10 in parallel

This is the observation that stands out most to us. In normal, non-Project sessions — same model, same maximum reasoning setting, no Routine — we have run as many as **10 parallel sessions at once and never hit this rate-limiting, at any point in time**. The throttle only appeared under the Projects beta, and specifically with more than one Project on the same repository. Because 10 parallel normal sessions never tripped it, raw concurrency alone does not seem to be the explanation; something about the Projects layer (or multiple Projects sharing one repo) appears to be the difference. We share this as the cleanest contrast we have, not as a proven mechanism.

### 4 — "not your usage limit" versus the granted beta access

The message explicitly states it is not our usage limit, and the team kindly granted elevated access for this beta — so from the user side it is genuinely unclear what limit is being reached. We are not assuming anything is wrong; we simply cannot tell whether the right response is to wait, to slow down, or to reduce the number of Projects on the repo. A one-line clarification would remove the guesswork.

### 5 — Sessions felt slower than a normal session

With the model and reasoning setting held constant, sessions run under the Projects beta felt noticeably slower than our normal, non-Project sessions — longer pauses between steps, observed many times rather than as a one-off. We could not benchmark it precisely, so we raise it as a perception to investigate, not a measured figure. It tended to coincide with the more-than-one-Project-per-repo condition above, which is why we wonder whether the signals share a root.

### 6 — Still present from round 2: agents never auto-terminate

Briefly, since it is already covered in round 2 and we are only re-flagging that it persists: review and worker sub-agents stayed "running" for very long stretches with little activity, never auto-terminated, and accumulated. Our only new angle is the hypothesis (open question above) that these never-terminated agents may hold concurrency and feed the slowness and throttling — raised as a hypothesis, not a claim. The full detail and suggested fix are in our round-2 note.

---

## Screenshots (attached with the email)

- **Inline rate-limit errors in a live session** — the running session showing two consecutive "Service was busy / Server is temporarily limiting requests (not your usage limit) · Rate limited" errors back to back (illustrates item 1).
- **Background-tasks panel(s)** — a large multi-agent run in the Projects beta, many sub-agents running in parallel at multi-hour runtimes with few tool uses: the load condition under which the rate-limiting and slowness appeared, and the never-terminated agents themselves (illustrates items 2, 5 and 6).
- **Desktop session view** — the same heavy multi-agent / multiple-Project run in context (illustrates item 2).

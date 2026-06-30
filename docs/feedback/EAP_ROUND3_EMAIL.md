# Round-3 Feedback — Email Body

**To:** tg@anthropic.com, claude-code-early-access@anthropic.com
**Subject:** Claude Code Projects (beta) — Round-3 feedback: transient rate-limiting with multiple Projects on one repo

---

Hi Tim (and team),

Thank you again for the elevated access on the Claude Code Projects beta — it's let us run real, heavy, multi-agent overnight batches we couldn't otherwise, and we're grateful. In the same spirit as rounds 1 and 2, this is a short round-3 note: questions to understand, not complaints. We may well be using the beta in an unusually heavy way, so please read everything below as "here's what we saw, and here's what we'd love your guidance on."

One new signal keeps recurring: transient rate-limiting — "Server is temporarily limiting requests (not your usage limit) · Rate limited" — which terminates sub-agents mid-task (the agent dies with no auto-resume, so its work is lost). The clearest pattern: it appears only when we have more than one Project open in parallel on the same repository; with a single Project we don't see it.

The contrast that stands out most: in normal, non-Project sessions — same model, same maximum reasoning setting, no Routine — we've run as many as 10 parallel sessions and never hit this once, at any point. So it doesn't look like raw concurrency; it seems specific to the Projects layer. Alongside it, sessions felt noticeably slower than a normal session, and (still from round 2) sub-agents never auto-terminate and just accumulate.

We'd genuinely love your guidance: what limit this is, whether opening more than one Project on the same repository is advisable, and a safe ceiling (Projects-per-repo, parallel-agent count) if so.

The full six observations, what we'd love, and our open questions are in the attached page, with three screenshots that illustrate each (the inline rate-limit error, the background-tasks panel, and the desktop session view).

Thank you for taking a look.

Warm regards,
Parthiban

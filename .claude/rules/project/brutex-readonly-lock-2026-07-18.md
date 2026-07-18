# bruteX Read-Only Lock — Operator Lock 2026-07-18

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F/§H > this file > defaults.
> **Companion rules:** `groww-second-feed-scope-2026-06-19.md` §32/§35/§37 (bruteX =
> reference-only, no code pulled; §37.3 producer-contract clarification in §1.2 below) +
> `brutex-crossverify-error-codes.md` — the S3 read leg is UNaffected by this lock:
> BruteX-produced S3 artifacts in OUR bucket remain a KEEP read class.
> **Scope:** PERMANENT. Every Claude Code / Cowork session, every worker, the prod box,
> AWS, CI, scripts — every tickvault-side actor. FOREVER.
> **Operator-locked:** 2026-07-18 (verbatim quotes below, typos included).
> **Mechanical enforcement:** procedural only today — no build-failing guard test exists
> for this lock; a ratchet (e.g. a source-scan test banning bruteX write API calls) is a
> flagged follow-up.
> **Auto-load trigger:** Always loaded (path is in `.claude/rules/project/`).

---

## §0. The verbatim operator demands (preserve exactly, do not paraphrase — typos included)

**Quote 1 (2026-07-18):**
> "it should be just read only access alone only... from this tickvault repository it should just access brutex repo alone right to read the indicators and strategies packages"

**Quote 2 (2026-07-18):**
> "in the future from tickvault live repo or claude code or aws whatever it is it should be always readable alone only... only read access"

---

## §1. The rule (one line)

**ANY tickvault-side actor (Claude/Cowork sessions, workers, the prod box, AWS, CI,
scripts) may READ the bruteX repo (`SJParthi/bruteX`) — the indicators/strategies
packages are the intended surface — and must NEVER write: no PRs, no pushes, no
branch create/delete, no issues, no PR/issue comments, no API mutations of any kind
against `SJParthi/bruteX`.**

## §1.1 The contract (mechanical)

| Aspect | Locked value |
|---|---|
| Allowed | READ-ONLY: clone/fetch/read-only attach (`add_repo` for read), file reads, `git log`/`diff`, read-class API GETs. Intended surface: the bruteX indicators + strategies packages |
| Forbidden | EVERY write path: `git push` to bruteX, PR open/edit/merge/close, branch create/delete, issue open/edit, PR/issue comments, reviews, labels, releases, workflow dispatches (`actions run` triggers), any mutating GitHub API call |
| Remote hygiene | NEVER register bruteX as a writable remote in any tickvault workspace; per-session read-only attach only when a session actually needs the packages |
| Session hygiene | `register_repo_root` for bruteX is FORBIDDEN — its ~1MB root CLAUDE.md would auto-load into session context, the exact context-overflow hazard that killed session spawning (and why bruteX is being removed from the project's DEFAULT repo list, with per-session read-only attach instead) |
| Code hygiene | bruteX stays reference-only — no code vendored into tickvault (`groww-second-feed-scope-2026-06-19.md` §32/§35 precedent stands) |
| Override protocol | Rule-file-first: the operator must update THIS file with a fresh dated quote BEFORE any bruteX write. A verbal approval alone = REJECT in review |

## §1.2 Clarification — the §37.3 "relay target" wording (producer contract)

`groww-second-feed-scope-2026-06-19.md` §37.3 calls the BruteX producer contract a
"relay target for the BruteX repo". Clarified under this lock: relaying anything INTO
the bruteX repo is OPERATOR-SIDE only — a tickvault session never writes/posts it
(this lock governs). The contract text stays here as the shared reference; delivery
to bruteX is the operator's action.

## §2. Precedent — the withdrawn split PR #1645 (2026-07-18)

Earlier the same day, a tickvault session had opened bruteX PR #1645 (a CLAUDE.md
split, coordinator-relayed as operator-ordered at the time) with native squash
auto-merge armed and CI near-green. Upon this ruling the auto-merge was DISARMED in
time and the PR was CLOSED un-merged (closed_at `2026-07-18T13:43:20Z`,
API-verified; withdrawal comment 5011479210 in full: "withdrawn — tickvault-side
access is read-only per operator 2026-07-18; split left to bruteX's own lanes if
ever wanted"). The stale
`claude/claude-md-split` branch deletion was itself refused by the closed bruteX
write path (HTTP 403) — consistent with this lock; it awaits operator/manual
cleanup on the bruteX side.

## §3. What a violating action/PR looks like (REJECT)

- Opens ANY PR, issue, comment, review, or branch against `SJParthi/bruteX` from a
  tickvault session, worker, CI job, or script.
- Pushes to (or force-pushes / deletes a branch on) bruteX.
- Dispatches a workflow (`actions run` trigger / `workflow_dispatch`) against bruteX.
- Registers bruteX as a writable remote, or stores bruteX write credentials, in any
  tickvault workspace or deploy path.
- Calls `register_repo_root` for bruteX (the ~1MB root CLAUDE.md context-overflow
  hazard — read-only attach without root registration is the allowed shape).
- Vendors bruteX code into tickvault (reference-only rule stands).
- Proceeds on a verbal "go ahead" without a fresh dated quote added to THIS file first.

Any such action MUST be rejected/aborted even if the operator approves verbally —
the operator must update this rule file FIRST with a dated quote.

## §4. Auto-driver / Insta-reel explanation

> Sir, the neighbouring shop (bruteX) keeps a recipe book we're allowed to READ —
> the indicators and strategies pages — whenever our juice shop needs them. But our
> boys may never write in that book: no corrections, no new pages, no sticky notes.
> Earlier today one boy had started rewriting their front page; the moment you ruled,
> he put the pen down and tore up his draft — read the book, never touch the pen.

## §5. Trigger / auto-load

Always loaded. Reinforced on any session that:
- References `brutex` / `bruteX` / `SJParthi/bruteX` in any file, plan, or prompt
- Calls `add_repo` (or any repo-attach mechanism) for bruteX
- Adds a git remote, push target, or GitHub API call targeting bruteX
- Edits `.claude/rules/project/brutex-readonly-lock-2026-07-18.md`,
  `brutex-crossverify*`, or any file containing `brutex_crossverify` /
  `BRUTEX-XVERIFY`

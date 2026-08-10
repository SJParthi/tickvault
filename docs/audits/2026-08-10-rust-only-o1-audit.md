# Rust-O(1)-Only Audit — 2026-08-10

> ## ▶ RESUME HERE (handoff for the next session)
>
> **Branch:** `claude/codebase-optimization-hardening-bg3oar` · **PR #1737** (DRAFT)
>
> | # | Item | Status |
> |---|---|---|
> | 1 | **CRITICAL** — gate-bypass routing in the dispatch hook | ✅ **FIXED + bite-tested** (this session) |
> | 2 | Guard vacuity: add a non-emptiness assert on the scanned corpus | 🔵 **OWNED BY THE PARALLEL SESSION** |
> | 3 | Guard case-sensitivity: lowercase before matching | 🔵 **OWNED BY THE PARALLEL SESSION** |
> | 4 | Pin both guard allowlists at length 0 so re-growing fails the build | 🔵 **OWNED BY THE PARALLEL SESSION** |
> | 5 | Pin the mutable action tags | ✅ **DONE** (3 of 4 pinned; the 4th is a different major with no verified SHA and is flagged in-file rather than guessed) |
> | 6 | Correct 3 stale claims in the master doc (2 O(1) entries + frontend-retired) | ✅ **DONE + guard re-verified** |
> | 7 | Close the second auto-merge arming path that never reads All Green | ✅ **DONE + bite-tested 6 cases** |
> | 8 | Tighten the over-broad secret-scanner line exclusion | ✅ **DONE + bite-tested 7 cases, 0 false positives on 400 real files** |
> | **0** | 🔴 **`live_feed_purity_guard` is VACUOUS ON DISK RIGHT NOW** — see §4c | ⬛ **TOP PRIORITY, needs a decision** |
> | 9 | **OPERATOR DECISION** — are 18 CI JavaScript blocks, 1 Perl gate and 94 shell scripts in scope for "Rust only"? | ⬛ BLOCKED on operator |
> | 10 | Vacuous-guard sweep across ALL `*_guard.rs` (its agent never launched) | ⬜ TODO |
>
> ⚠ **TWO SESSIONS ARE WORKING THIS REPO.** A parallel session ("Market data
> platform analysis") is mid-edit on `crates/common/tests/rust_only_guard.rs`
> (items 2/3/4). This session has deliberately NOT touched that file — two
> sessions editing one file is how work gets lost. Confirm which branch that
> session targets before merging either. Note also that it reported the pinned
> toolchain's standard library missing in its container and fell back to stable
> 1.94.1, while CI pins 1.95.0 — its local green is therefore NOT the final
> word; CI on the pinned toolchain is.
>
> **Before running any cargo command:** check `df -h /` first. Each commit triggers
> the invariant test, which rebuilds `target/` to ~28 GB against a ~38 GB quota.
> Recovery when it fills: delete the agent transcript files under the session
> tasks directory, then `cargo clean --profile dev -p <crates>`. This filled the
> disk FIVE times in one session.

> **Status:** EVIDENCE + PARTIAL REMEDIATION. The operator authorised fixes
> ("fix and work and implement everything"), then asked for the work to be pushed
> so it can resume in a fresh session. **Item 1 — the only CRITICAL — is fixed and
> verified.** Items 2-8 and 10 remain; item 9 is a policy decision only the
> operator can make. This document is the handoff.
>
> **Scope of the operator's ask (2026-08-10):** *"Ensure to use one and only RUST
> O(1) in the entire workspace codebase except frontend alone so check this every
> nook and corner with assurance and guarantee."*
>
> **Method:** six parallel adversarial agents, source-level only. Every finding
> below carrying **[V]** was independently re-verified by the coordinating session
> with its own command, not taken on an agent's word. **[A]** = agent-reported,
> not yet independently re-verified.
>
> **Hard constraint during this audit:** `target/` was empty and ~7.7 GB disk free
> (a build needs 24 GB+), so NO cargo build/test/check was run. Every claim here is
> from reading source. Anything requiring execution to confirm is marked as such.

---

## §1. The headline

**The Python purge genuinely held. The Rust-only lock did not.**

Tracked `.py` files: **0** [V] (`git ls-files '*.py' | wc -l` → `0`). The
2026-07-31 zero-Python directive is intact.

But "Rust only, everywhere" is **not** true today, and the guard that is supposed
to enforce it can pass while the lock is violated. The non-Rust surface that
actually executes is large and almost entirely outside the guard's scope.

---

## §2. `rust_only_guard.rs` — the guard is structurally unsound

Target: `crates/common/tests/rust_only_guard.rs`

### §2.1 VACUITY — CONFIRMED [V]

Every real-tree assertion asserts the **violation set** is empty. **Nothing
asserts the scanned corpus is non-empty.**

- `grep -cE "assert!\(!.*is_empty\(\)\)|assert!\(.*len\(\) *> *0"` → **0** [V]
- If `load_invocation_scan_files()` (L305–316) or `git_ls_files(&["*.py"])`
  (L324/L339) ever returns `vec![]`, the guard reports GREEN while enforcing
  nothing.
- Not live today (162 files currently match), but structurally unguarded: a glob
  change, a directory rename, or a CWD shift silently empties it.
- The doc-comment at L377 claims `guard_self_test` proves non-vacuity. It does
  **not** — that test injects synthetic lists and never touches the real tree.
  **A comment asserting a guarantee the code does not provide.**

### §2.2 CASE-SENSITIVITY — CONFIRMED [V]

`grep -c "to_lowercase\|to_ascii_lowercase\|eq_ignore_ascii_case"` → **0** [V].
Matching is byte-exact, so every one of these evades the ban outright:

    PIP3 install awscli
    Poetry run ...
    UV pip install ...

### §2.3 Scope holes — files the guard never opens

| # | Unscanned surface | Evidence | Sev |
|---|---|---|---|
| 1 | `.claude/settings.json` — contains **18 live `"type":"command"` hooks** [V] | guard scans only `.mcp.json` (L121); sibling `tickvault_logs_mcp_guard.rs` also omits `.claude/` | CRITICAL |
| 2 | Extensionless executables (`scripts/git-hooks/*`, any `scripts/mytool`) | in neither the `.py` list nor the extension branch (L97–125) | CRITICAL |
| 3 | systemd units (`deploy/systemd/*.service`) | no `.service` branch | HIGH [A] |
| 4 | `.tf.json` / other JSON | only `.tf` handled (L117) | MEDIUM [A] |
| 5 | `.md` with executable blocks | out of scope (L38) — the exact shape of the deleted `dhanhq` skill tree | MEDIUM [A] |

### §2.4 Token-set holes

- **`pipenv` evades `pip`** [A] — the digit-check after `pip` sees `'e'`, fails
  `after_ok`, and the scan restarts past the match.
- Missing package managers: `micromamba`, `mamba`, `pdm`, `rye`, `hatch`,
  `pixi`, `ensurepip` [A].
- **Only one runtime is banned.** `node`, `deno`, `bun`, `ruby`, `Rscript`,
  `perl`, `awk`-as-language are all unbanned [A].

### §2.5 The "hard-zero floor" is review-only, not mechanical [V]

    const TRACKED_BANNED_ALLOWLIST: &[&str] = &[];      // L61
    const INVOCATION_SITE_ALLOWLIST: &[&str] = &[];     // L67

Both are empty **today** [V] — good. But nothing pins them at length zero, so a
future PR can re-add an entry *and its own allowlist exemption* together and stay
green. The lock's claimed "HARD ZERO floor" is enforced by reviewer discipline,
not by the build.

---

## §3. What actually executes non-Rust today

Ranked by blast radius. Frontend exclusions applied per §4.

> **Correction applied after cross-agent review.** An earlier draft of this
> section implied the CI JavaScript gates merges. **It does not.** `ci.yml` — the
> only workflow feeding the `All Green` merge choke point — is interpreter-free
> end to end (`grep -cE "github-script|python|npx | node |perl " ci.yml` → **0**)
> [V]. Every one of the 18 JS blocks lives in `schedule:`/`workflow_dispatch`
> workflows (`safety.yml` additionally on `push`) [V], none of which is in
> `all-green`'s `needs:` list. Severity is downgraded accordingly. Reporting it
> as a merge-path violation would have been false.

| Surface | Count | Merge-blocking? | Class |
|---|---|---|---|
| `actions/github-script` — inline **JavaScript** in CI | **18 sites** / 5 workflows [V] | **NO** [V] | VIOLATION (non-merge-path) |
| **Shell dispatched to the PROD box from Rust** | `operator_control.rs:1722` `AWS-RunShellScript` [V]; 13 dispatch sites | n/a | VIOLATION — but **POSIX shell only, no interpreter** [A]; the 2026-08-01 remediation HOLDS |
| **Perl** gating every terraform apply | `terraform-apply.yml:290` [V] | No (infra lane) | VIOLATION — known/accepted |
| Shell scripts | **94 tracked `.sh`** [V] + 3 extensionless git hooks | n/a | VIOLATION (hooks arguably dev-tooling) |
| `user-data.sh.tftpl` — 301-line boot script | `.tftpl` defeats `*.sh` globs [A] | n/a | VIOLATION — boots every EC2 |
| `npx @modelcontextprotocol/*` — Node | `.mcp.json` [A] | No | EXEMPT-TOOLING (dev only) |

**The point that survives the correction:** a guard scanning only `.py` catches
**none** of these. The largest non-Rust *language* surface in the repo is
invisible to every extension-based scan — that is a real gap even though it is
not on the merge path.

### §3.1 NEW — 4 unpinned GitHub Action tags [V]

CLAUDE.md requires every version pinned (`:latest`, `^`, `~`, `*` are BANNED).
`fuzz.yml` and `safety.yml` correctly SHA-pin `actions/github-script`. These do
not:

    dep-freshness-nightly.yml:180   actions/github-script@v7
    dep-freshness-nightly.yml:229   actions/github-script@v7
    chaos-nightly.yml:187           actions/github-script@v7
    full-test-nightly.yml:118       actions/github-script@v8

A mutable tag means the JS that writes issues can change under us without a diff.

### §3.2 Highest-risk single behaviour [A]

`safety.yml` (6 sites) **auto-CLOSES** sanitizer/careful issues when a run goes
green. If it misfires it closes a real open finding — a silent regression, and
the worst failure mode among the 18 because it destroys signal rather than
merely missing it.

## §4. Does a frontend exist? YES — and CLAUDE.md is stale on this [V]

Four live frontend surfaces, ~1,053 lines of browser JavaScript:

| File | Lines [V] |
|---|---|
| `crates/api/src/handlers/dashboard_page.rs` | 546 |
| `crates/api/src/handlers/feeds_page.rs` | 667 |
| `crates/api/src/handlers/board_page.rs` | 558 |
| `crates/aws-lambdas/src/operator_control_console.html` | 563 |

These are the **legitimate `EXEMPT-FRONTEND` set** under the operator's carve-out.

**Correction required in CLAUDE.md:** it states "the entire `/portal/*` HTML
frontend was retired 2026-05-19". That is true of `/portal/*` specifically but
**stale as a global statement** — the operator console and these three API pages
postdate it.

**Nothing else qualifies as frontend.** The SSM shell payloads run on a server;
`github-script` runs on a CI runner; the 12 vendor GDF `.html` dumps are never
served. None of those can be waved into the frontend exemption.

---

## §4a. O(1) — the claims are HONEST, the documentation is STALE

**Good news first, because it is the larger finding:** the audit found **zero**
cases of an `O(1)` comment sitting above non-O(1) code [A]. The codebase is
systematically honest in the opposite direction — e.g. `truedata.rs:44-47`
explicitly states *"The field extraction is O(1); the frame is not… Calling the
per-frame decode O(1) is a REJECT"*, and `multi_tf_aggregator.rs:372-377` records
a prior FALSE claim of its own and its correction. Identity lookup, indicator
slot allocation and aggregator slot allocation are all genuine O(1)-average hash
paths, and slot exhaustion **fails closed and loud** (counter + error naming the
consequence), not silently.

**But CLAUDE.md's non-O(1) exception list is stale in two ways** [A] — and that
list already warns of itself that it was found incomplete once before
(2026-08-09) and is "audited-as-of", not exhaustive:

| # | Drift | Direction |
|---|---|---|
| 1 | `spot_bar_store.rs` is documented as doing an "O(#slots ≤ 256) linear scan on EVERY read and write". It no longer does — `find_slot` is now a `HashMap` O(1)-average lookup (changed 2026-08-10). | CLAUDE.md **over-states** cost — safe direction, but wrong |
| 2 | `crates/app/src/rest_candle_fold.rs` `FoldSlots` is **missing from the list entirely**. It was an O(bars × slots) scan — "at 25,000 instruments degrades to effectively O(n²) per minute" — until repaired 2026-08-09. It is the currently-live REST fold path. | **Omission**; now O(1)-avg, so a doc gap, not an active violation |

**Not verifiable without execution:** all absolute latency figures (e.g. "~200ns
on t4g.medium"), DHAT zero-alloc assertions, and the memory ceilings. No bench or
test was run, so this audit makes no runtime claim about them.

## §4b. Enforcement-chain security — one CRITICAL bypass [V]

The fifth agent completed after the first commit of this document. Its top
finding was re-verified by reading the source directly.

### CRITICAL — a newline defeats the compound-command guard

`.claude/hooks/pre-tool-dispatch.sh`

Two independent defects combine into a full bypass of the pre-commit battery:

1. **The separator alternation (L21) lists only `&&`, `||` and semicolon —
   newline is absent.** `grep` is line-oriented, so in a two-line command
   neither line contains a separator followed by a gated verb, and **the guard
   does not fire.** (Verified: this same guard DID correctly block an
   ampersand-joined stage-then-commit during this session, so the documented
   path works and only the newline path leaks. It also blocked an early draft
   of this very section for quoting its own patterns — the matcher is
   content-blind.)

2. **Routing (L47-55) is first-substring-match-wins, and the push arm is
   tested BEFORE the commit arm**, each arm ending in `exit $?`. Neither test
   is anchored, so a command containing BOTH verbs matches the push arm first
   and routes to the push gate **only**; the commit gate is never reached.

**Consequence:** a single Bash call carrying a commit on one line and a push on
the next runs **neither** the compound block **nor** `pre-commit-gate.sh`. That
skips fmt, banned-pattern scan, data-integrity, O(1)/dedup scan,
**secret-scanner**, version-pinning, commit-message validation and the
commit-time invariant test. Worse, `pre-push-gate.sh` computes its diff against
`HEAD` *before* the pending commit exists, so the new commit's content is
invisible to it as well. CI is the only remaining backstop.

### ✅ FIXED 2026-08-10 (verified)

The router now runs **both** gates, commit first, whenever a command carries both
verbs. The fix targets the SECURITY property (every applicable gate runs) rather
than the cosmetic rule (one verb per call), so it holds regardless of which
separator is used — newline included.

Bite-tested across five shapes; normal routing is unchanged and only the bypass
shape changed behaviour:

| Command shape | Before | After |
|---|---|---|
| plain commit | commit gate | commit gate *(unchanged)* |
| plain push | push gate | push gate *(unchanged)* |
| **commit + newline + push** | **push gate ONLY — the bypass** | **BOTH gates** |
| pr create / pr merge | their own gates | *(unchanged)* |
| unrelated command | no gate | *(unchanged)* |

**Accepted cost, recorded not hidden:** a commit whose message merely QUOTES these
verbs (e.g. documentation about them) now pays one extra gate run. Wasted seconds,
never a wrong answer, and it fails in the safe direction — a redundant check
rather than a skipped one. The compound-block regex above was deliberately left
alone: teaching it to treat newline as a separator would block legitimate
documentation commits, and with both gates now running it is no longer
load-bearing for security.

### HIGH — a second auto-merge arming path that never reads All Green [A]

`.github/workflows/auto-merge.yml:46-68` — the manual-dispatch fallback arms
GitHub auto-merge after checking only same-repo origin and non-draft. It **never
consults the `all-green` job result**. That is a side door around the merge-gate
lock's central control and reproduces the 2026-07-03 red-merge class. The file's
header credits "the same guard from PR #1390", but that guard was the fork/draft
check only.

### The All-Green fan-in itself reviewed CLEAN [A]

`needs:` covers every job defined in `ci.yml` (no orphan job that runs without
gating); a skipped result fails the gate except the documented
`{commit-lint, design-first-wall, local-runtime-block}` carve-out on non-PR
events; a missing/null job result renders `None` and is treated as failure. No
script injection (`pull_request_target`, unsanitised PR title/body interpolated
into a run block) and no `permissions: write-all` anywhere in `.github/workflows/`.

### Lower-severity, both NEW [A]

- `aws-control.yml:426-441` — SSM values decrypted into a plain shell variable
  and passed as a CLI argument. Because the value never flowed through the
  `secrets:` context, Actions' automatic log-masking does **not** apply; a future
  trace flag or an argv-echoing error would print real credentials into the log.
- `.claude/hooks/secret-scanner.sh:79-88` — a line containing `example`/`test_`/
  `mock`/`stub`/`fixture`/`dummy` **anywhere** is excluded, so a real token on a
  line carrying a trailing "stub for tests" comment passes both the local
  scanner and the CI lane that reuses the same script.

## §4c. 🔴 A guard that is GREEN while enforcing NOTHING — today, on disk

A dedicated vacuity sweep examined **162 guard files**. The denominator matters:
**138** assert against explicitly-named single files and are structurally sound
(a missing file fails the read). **24** walk a corpus. Of those 24: **5 sound**,
**12 vacuity-prone**, and **1 CONFIRMED VACUOUS RIGHT NOW**.

### The confirmed one: `crates/storage/tests/live_feed_purity_guard.rs` [V]

All three of its scan roots **no longer exist**:

    ❌ crates/core/src/historical
    ❌ crates/core/src/rest
    ❌ crates/core/src/backfill

and the loop skips anything missing:

    for dir in historical_flow_paths() {
        if !dir.is_dir() { continue; }      // <-- all three skip here

so `violations` stays empty and the test **passes while checking zero files**.

**Why nobody noticed — the part that matters most.** The guard HAS an
anti-vacuity self-test, and it is a **tautology**:

    let paths = historical_flow_paths();
    assert!(paths.iter().any(|p| p.ends_with("historical")));

`historical_flow_paths()` returns a hardcoded list. Asserting that list contains
a path ending in "historical" is true **regardless of whether that directory
exists**. The check designed to prove the guard was working is precisely why the
rot went undetected. **A fake non-vacuity test is worse than none**, because it
converts "unverified" into "verified" on the status board.

**Why it is acute, not theoretical:** this guard exists to stop `append_tick(` /
`TickPersistenceWriter` from re-entering a historical/REST flow and writing
synthetic ticks into the real `ticks` table. The 2026-08-09 Dhan revival plan
mandates **rebuilding `tick_persistence.rs` and the REST legs** — exactly the
code this guard was built to police. It would be rebuilt with the guard asleep.

**NOT fixed here, and the reason is honest rather than convenient.** The correct
repair is to repoint the scan roots at today's REST/persistence modules and make
a missing root a hard failure instead of a `continue`. But *which* paths are
correct depends on the Dhan revival plan, which is still **DRAFT and unapproved**
— and I cannot verify a `crates/storage` test change without a build the current
disk cannot afford. Pushing an unverified change to a security guard, on a guess
about an unapproved plan, would be worse than the disclosed gap. It needs a
decision plus a build budget.

### The 12 vacuity-prone siblings [A]

Same family, not yet fired. The two most consequential:

| Guard | Trigger | Invariant that would silently go unenforced |
|---|---|---|
| `error_level_meta_guard.rs:94` | `let Ok(entries) = fs::read_dir(dir) else { return; }` + a root fallback that silently mis-roots on any layout change | Rule 6 — flush/persist failures must be `error!`, not `warn!`. A downgrade stops paging → silent data loss |
| `error_code_tag_guard.rs:64` | same swallow + `.unwrap_or_else(\|\| PathBuf::from("."))` CWD fallback | Rule 5 — every `error!` carries its code. Untagged errors never match the CloudWatch metric filters, so the 9 paging codes go dark |

Both also carry **fake non-vacuity tests** of the same shape: they assert a
constant the test file itself declares is non-empty, never that a single source
file was actually scanned.

**The one-line pattern fix for all 12:** replace
`let Ok(entries) = fs::read_dir(dir) else { return; }` with an
`.unwrap_or_else(|e| panic!("guard corpus unreadable: {dir:?}: {e}"))`, and add a
real `assert!(files_scanned > 100)` to each walker. `dhan_exit_order_lockout_guard.rs:157`
already proves that pattern works in this repo.

## §5. Recommended fix order (NOT applied — operator's call)

1. **Invert the guard's scope.** Replace the extension allow-list with *scan every
   tracked text file except an explicit ratcheted exclusion list*, and add
   `assert!(!files.is_empty())` to both real-tree tests. This single change kills
   §2.1 and all five §2.3 holes at once.
2. **Lowercase before matching** (§2.2) — one line, kills the `PIP3` class.
3. **Pin both allowlists at length 0** so re-growing them fails the build (§2.5).
4. **Decide the JS/shell policy explicitly.** §3 is not a bug list — it is a
   scope question only the operator can answer: are 18 CI JavaScript blocks, a
   Perl gate, and 94 shell scripts *in* scope for "Rust only"? If yes, that is a
   large migration. If no, the lock's wording should say so, because today the
   rule file and the repo disagree and the guard hides the disagreement.

**Honest note on #4:** items 1–3 are mechanical and safe. Item 4 is a
policy decision with real cost, and this audit deliberately does not presume it.

---

## §6. Audit provenance

Six agents dispatched; **five launched** (one — the vacuous-guard hunter — failed
on a transient model-availability error, not a real fault). **Four completed and
are folded in:** interpreted-language census, guard bypass hunt, O(1) hot-path
verification, CI/deploy non-Rust sweep. **All five that launched have now
completed** — the enforcement-chain security review landed after the first commit
of this document and is folded in as §4b. **Still NOT covered:** the
vacuous-guard sweep across all `*_guard.rs` files, whose agent never launched. A
later session should run that dimension; `rust_only_guard.rs` is confirmed
vacuity-prone (§2.1) and its siblings were never checked for the same pattern.

Cross-agent review changed two conclusions before commit: the CI JavaScript was
demoted from "merge-blocking" to "not on the merge path" after direct
verification, and the SSM shell payloads were confirmed interpreter-free. Both
corrections made this report *less* alarming than the first draft.

**Not claimed:** anything requiring execution. No cargo build/test/bench ran, so
no runtime, timing, or coverage claim is made here. The O(1) dimension in
particular is only partially covered — CLAUDE.md's own non-O(1) exception list
warns it was found incomplete once already (2026-08-09) and is stated as
"audited-as-of", not exhaustive.

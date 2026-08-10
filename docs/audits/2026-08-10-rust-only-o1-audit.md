# Rust-O(1)-Only Audit — 2026-08-10 (findings only; NO fixes applied)

> **Status:** EVIDENCE HANDOFF. Nothing in this file has been fixed. The operator
> deferred remediation to a later session ("we will take care of this in another
> session"). This document exists so that session starts from verified evidence
> instead of re-running the audit cold.
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
verification, CI/deploy non-Rust sweep. **One was still running at commit time:**
the enforcement-chain security review (hook/CI bypass surface, All-Green fan-in
integrity, secret exposure, script injection). **That dimension is NOT covered
here** and a later session should re-run it, along with the vacuous-guard sweep
across all `*_guard.rs` files that never launched.

Cross-agent review changed two conclusions before commit: the CI JavaScript was
demoted from "merge-blocking" to "not on the merge path" after direct
verification, and the SSM shell payloads were confirmed interpreter-free. Both
corrections made this report *less* alarming than the first draft.

**Not claimed:** anything requiring execution. No cargo build/test/bench ran, so
no runtime, timing, or coverage claim is made here. The O(1) dimension in
particular is only partially covered — CLAUDE.md's own non-O(1) exception list
warns it was found incomplete once already (2026-08-09) and is stated as
"audited-as-of", not exhaustive.

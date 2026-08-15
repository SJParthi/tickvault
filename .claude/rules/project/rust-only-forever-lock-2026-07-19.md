# Rust-O(1)-Forever — Operator Lock 2026-07-19

> **Authority:** CLAUDE.md > `operator-charter-forever.md` > `hot-path.md` > this file > defaults.
> **Scope:** PERMANENT. Every environment, every PR, every task, every current and future Claude/Cowork session.
> **Operator-locked:** 2026-07-19 (verbatim demand below).
> **Companion enforcement:** `crates/common/tests/rust_only_guard.rs` (the phase-3 shrinking-allowlist ratchet this lock rides with).
> **Auto-load trigger:** Always loaded (path is in `.claude/rules/project/`).

---

## §0. The verbatim operator demand (preserve exactly, do not paraphrase — typos included)

> "what else is remianing is our entire system became entirley rust dude not only now even in the future whenever it ry to provid enay requirmenets discussiosn or it could be anyhtig dude whatevr it is by default it needs to ebcome RUST O(1) dude okay? can that happen dude?"

**Quote 2 (2026-07-31 — zero tracked interpreted-language files; preserve EXACTLY, typos included):**

> "ensure to use only RUST O(1) entolrwy everywhere bro I mean entire workspace codebase everything entirely bro okay? I need the gauarntee and assurnace bro see nowhere the word python shoudl be available dude okay?"

Effect (executed in the PR carrying this edit): **every tracked `.py` file is
DELETED — the tree is at ZERO** (17 files: the 15-file `.claude/skills/dhanhq/`
vendor-SDK reference tree, deleted whole since it was an interpreted-language
SDK reference end-to-end — 95 fenced code blocks, `pip install` — plus 2
historical incident repro scripts). BOTH ratchet allowlists in
`crates/common/tests/rust_only_guard.rs` are emptied to `&[]`
(`TRACKED_PY_ALLOWLIST` and `INVOCATION_SITE_ALLOWLIST`), so the shrinking
ratchet is now a HARD ZERO floor: any new tracked `.py`, and any new
interpreted-language invocation token in a shell script / workflow yml /
Makefile / `.mcp.json` / terraform template, fails the build. The 3 former
invocation-allowlisted files carried only the WORD in comments (no live
execution existed) — the 3 flagged mentions were reworded so the allowlist
could go empty. **[⚠ THAT PARENTHETICAL WAS FALSE — see the 2026-08-01
correction block immediately below §0.]** `docs/dhan-ref/*.md` (21 files) is now the SOLE Dhan API
reference; CLAUDE.md's "KEY FILES" row + skill note are updated in lockstep.

> ## ⚠ 2026-08-01 CORRECTION — "no live execution existed" was FALSE
>
> A four-agent audit (operator directive, 2026-08-01: *"nowhere i shdpou le
> even see the pyhton word itsefl"*) found **ELEVEN live interpreted-language
> invocation sites** that had been passing this guard GREEN the whole time —
> because the token set banned the interpreter's OWN name and never its
> package manager:
>
> | Site | What actually ran |
> |---|---|
> | `.github/workflows/terraform-apply.yml:233,408` | `pip3 install --break-system-packages ziglang==0.14.1` — `cargo-zigbuild` then invoked the interpreter as the **arm64 LINKER of every production Rust lambda** |
> | `scripts/bootstrap.sh:114-117,123` | `pip3 install awscli` (the deprecated **v1** wheel) |
> | `scripts/provision-infra-secrets.sh:36-42` | same |
> | `scripts/setup-secrets.sh:38,42` | same |
> | `scripts/setup-observability.sh:171` | same (help text) |
>
> **Fixed in the same PR:** the ziglang wheel → the OFFICIAL upstream zig
> tarball (same pinned 0.14.1, zero interpreters, fails loudly rather than
> falling back); all four AWS-CLI paths → `scripts/ensure-aws-cli.sh` (official
> self-contained installer, v2 not v1); and `banned_tokens()` in
> `rust_only_guard.rs` widened to `pip` / `pipx` / `uv` / `uvx` / `poetry` /
> `conda` / `virtualenv`. **Bite-proven:** re-adding the exact `pip3 install
> ziglang` line fails `no_new_banned_invocations` (verified 2026-08-01).
>
> **Still present, each needing its own decision — NOT silently allowlisted:**
> `perl -ne` (`terraform-apply.yml:282`, the non-ASCII security-group
> description guard), `rm -rf …/venv` (`deploy-aws.yml:729` — a cleanup line
> that only DELETES), and `npx @modelcontextprotocol/*` (`.mcp.json`, dev-only,
> never deployed). None is the banned runtime; all three are recorded here
> rather than hidden, and adding their tokens would fail the guard today.
>
> **Lesson binding on every future ratchet:** a ban on a runtime that permits
> its package manager is not a ban. Any future interpreter ban MUST enumerate
> the ecosystem's INSTALL verbs, not just the binary name.
>
> ### ⚠ 2026-08-01 (same day, second pass) — TWO EXECUTION sites, and the word itself
>
> The operator repeated the directive three times, escalating: *"nowhere i
> shdpou le even see the pyhton word itsefl"*, *"no python bridge pyhto. word
> python code ntohign shodu lbe fuckign bro okay?"*, *"i just need only RUST
> O(1) can you change it entilrey ?"*. Acting on it found TWO sites that were
> not a rewording problem — both genuinely EXECUTED the banned runtime, and
> both are now retired:
>
> | Site | What it did | Disposition |
> |---|---|---|
> | `crates/tickvault-logs-mcp/tests/parity.rs` | `materialize_server_py()` resurrected the DELETED implementation from pinned git history ONTO DISK and executed it; `python3()` hard-FAILED rather than skipping when absent. Ran in CI on every PR via `Test (logs-mcp)`. | **DELETED**, with its lockstep guard INVERTED |
> | `aws-lambdas/.../operator_control_action_commands.rs` `WIPE_QUESTDB_COMMANDS[6]` | a 17-line embedded program dispatched via SSM RunCommand to the PROD box, truncating QuestDB tables | **re-expressed as curl + POSIX shell**, same semantics, same stdout markers |
>
> **This retroactively qualifies the 2026-07-31 "the tree is at ZERO" claim
> above:** it was true at rest, but the parity harness wrote a file back at
> runtime, so the tree was not zero while its own test suite ran.
>
> **Honest cost of the harness retirement (a real coverage reduction, not a
> free deletion):** the load-bearing algorithm pins survive as self-contained
> golden literals in `src/` — hash vectors, the SigV4 signing-key and request
> goldens, the ensure_ascii goldens, the novel-cutoff overflow bands — none of
> which spawns anything. What is LOST and NOT replaced: the end-to-end
> JSON-RPC envelope diff, the `tools/list` registry diff against the legacy
> oracle, and transcription-error detection (a golden mis-copied in 2026-07-18
> is now invisible, because the remaining tests assert against the copy rather
> than the source).
>
> **Wire values keep their bytes.** Six string literals crossed a network
> boundary and could not be reworded: the Groww NATS CONNECT `lang`, the
> `x-client-platform` mint header, and four MCP tool-error / `inputSchema`
> strings. Each is byte-assembled through a const `core::str::from_utf8` so
> the bytes are IDENTICAL and only the literal leaves the source, and each is
> pinned by a test whose EXPECTED value is also spelled as bytes.
>
> **New ratchet:** `tickvault_logs_mcp_guard.rs::
> parity_harness_is_retired_and_nothing_spawns_the_legacy_runtime` fails the
> build if the harness returns OR if the word reappears anywhere in `crates/`,
> `scripts/`, `.github/`, `deploy/`, `Cargo.toml`, `quality/` or `.gitignore`.
> Bite-proven 2026-08-01. The two enforcement files are exempt from their own
> scan and both byte-assemble the token, so **our own source is at literal
> zero** — the class-1 carve-out below is now satisfied by construction rather
> than by exception.

**Honest boundary of Quote 2 (recorded, not hidden — §2's no-false-OK rule):**
"nowhere the word python" is satisfied for **executable files and invocation
sites** — the enforceable, mechanically-ratcheted surface. The literal string
still appears in three deliberately-retained classes, because deleting it
there would destroy the very thing the operator is asking for:
1. **`rust_only_guard.rs` itself** — the guard must name the banned token to
   ban it. Removing the word DELETES the enforcement.
2. **Migration-provenance comments** in the ported Rust (~768 comment lines,
   e.g. `alarm_gate.rs`'s "Python parity: `(event or {}).get('mode','close')`")
   — the audit trail proving each port is behaviourally faithful to what it
   replaced. This is the RECORD OF THE PURGE SUCCEEDING.
3. **Vendor API reference docs** (`docs/dhan-ref/`, `docs/groww-ref/`,
   `docs/gdf-ref/`, `docs/broker-ref-upload-*`) and historical plans/audit —
   third-party documentation describing THEIR SDKs, and dated history that
   house convention never rewrites.

Purging those three classes is REJECTED as self-defeating. Any future PR that
strips the token from class 1 or 2 must be rejected in review: it removes
enforcement or provenance while appearing to advance the directive.

---

## §0.1. 2026-08-14 — AUDIT: the workspace IS Rust-only; the GUARD has four scope holes

A hostile sweep re-verified the lock end to end (read-only; no build). **Verdict:
YES, the workspace is Rust-only today.** Zero tracked files in any of the 22
banned extensions; both allowlists empty and mechanically pinned at zero; all 99
tracked shebangs are `bash`; every `Command::new` literal is a benign binary;
the inline-JS budget matches the 4 sanctioned frontend surfaces exactly; and
every `pip`/`perl` hit in an executable surface is a `#`-comment recording a
completed port.

**But the guard's coverage gap is SCOPE, not tokens — the same class that
produced both prior breaches.** It decides what to read from a hardcoded list of
extensions plus one directory prefix, so several re-entry paths were green by
construction.

**CLOSED in this change:**

| # | Hole | Fix |
|---|---|---|
| 1 | An extension-less `#!` executable outside `scripts/git-hooks/` (`tools/deploy`, `bin/run`) was neither extension-banned nor invocation-scanned | `has_interpreter_shebang` — **any** tracked file whose first line is `#!` is now scanned, whatever it is called |
| 2 | `.bash` / `.zsh` / `.ksh` / `.ps1` / `.bat` escaped the `.sh` check — the one-rename evasion the `.pyw`/`.pyi` additions closed for the interpreter's own extensions | subsumed by #1 |
| 3 | `.cargo/config.toml` `[target.*] runner` / `linker` **executes on every build** and was structurally unscanned. This is not hypothetical: §0's 2026-08-01 correction records an interpreter package having actually BEEN the arm64 linker for every production Rust lambda | `.cargo/config.toml` + `Cargo.toml` added to the scan |

Hole #1 is the structural one and deliberately replaces enumeration with a
question about the FILE rather than its NAME — because the enumerate-one-more-
extension approach has now been wrong four times, always in the same direction:
a class nobody listed is invisible, and invisibility reads as green.

**CLOSED in the SECOND sweep of the same day** (a follow-up adversarial audit
run specifically to try to sneak a non-Rust executable past the just-widened
guard — it found four more, three of which needed no exotic technique):

| # | Hole | Fix |
|---|---|---|
| 6 | **`.args([…])` was invisible to the Rust spawn scan.** The marker set was `Command::new("` and `.arg("`; the PLURAL form contains neither, because an `s` sits between `arg` and the paren. So `Command::new("env").args(["<interpreter>", "-c", …])` was **fully literal and fully green** — the extractor saw only the benign `"env"` and never looked at the payload. `.args([…])` is the DOMINANT form in this workspace (20+ sites, including `build.rs`, which executes on every build) | `extract_spawn_literals` takes every string literal inside the bracket group, bounded at `]`; bite-proven both directions in `guard_self_test` |
| 7 | **Make's other names.** The check was `path == "Makefile"`, case-sensitive and single-name. GNU make searches `GNUmakefile`, `makefile`, `Makefile` **in that order**, so a tracked `GNUmakefile` SHADOWS the scanned `Makefile` entirely — and make files carry no shebang, so fix #1 above could not rescue them | `GNUmakefile` / `makefile` / `*.mk` added, plus `*.Dockerfile` (the `docker build -f prod.Dockerfile` convention, which `Dockerfile.*` does not match) and `*.json.example` / `*.json.template` (tracked seeds carrying hook COMMAND lines) |

**Recorded as an HONEST LIMIT rather than closed — a wrapper function defeats
the spawn scan, and the wrapper already exists.**
`crates/tickvault-logs-mcp/src/tools.rs::run_with_timeout(program, …)` is
called with bare `"bash"` / `"git"` / `"docker"` literals that sit in neither
marker form; a new call site passing an interpreter name would pass green.
Closing this needs call-graph analysis, not a string scan, so it is stated at
the function (`HONEST LIMIT 2`) rather than pretended away. The shebang
fallback and the file-extension ban still apply to whatever such a wrapper
launches.

**Also recorded, not closed:** inline JavaScript inside `.rs` string literals
is unbudgeted. The three `crates/api/src/handlers/*_page.rs` surfaces carry
~726 JS lines in raw strings; `.rs` is excluded from token scanning by design
and the spawn scan is literal-only, so a FIFTH browser surface — or unbounded
JS growth inside the existing three — is structurally invisible. The
"4 surfaces" figure in CLAUDE.md is prose enforced by nothing, while inline JS
in `.yml` **is** budgeted (`GITHUB_SCRIPT_BUDGET`). That asymmetry is a real
gap; closing it needs a budget const and an operator ruling on the 12
legitimate vendor-reference `.html` files under `docs/`.

**CLOSED 2026-08-15 — the node-family gap (was open item 4).**

`node` / `npx` / `npm` / `yarn` / `pnpm` / `deno` / `bun` were never banned
tokens, and `.mcp.json` runs `npx` live. The gap sat open because both obvious
fixes were wrong:

- **Adding them to `banned_tokens()`** fails the build on `.mcp.json` itself —
  dev-session MCP tooling that never reaches the box. Breaking local tooling to
  satisfy a lock that exists to protect the RUNTIME is the wrong trade.
- **A plain word-boundary scan** would flag `scripts/aws-autopilot.sh`'s three
  "SSM managed node" lines. A guard whose first act is three false positives
  teaches the reader that the cheapest fix is to allowlist it — the same
  dynamic that has weakened three anchors in this branch already.

The shape that is right for both: scan **command position**, not free text. The
token must BEGIN a command — line start, after a pipe / `&&` / `;` / `$(`, or
as a JSON `"command":` value. `managed node` fails that test; `npx -y pkg`
passes it. Then a shrink-only budget (`NODE_RUNTIME_BUDGET`, the
`GITHUB_SCRIPT_BUDGET` shape) pins the two existing `.mcp.json` entries so they
cannot grow, while a NEW node-family invocation anywhere fails the build.

Bite-proven in `guard_self_test` in BOTH directions: six real invocation forms
must count, and six mention-forms — including all three real "SSM managed node"
lines from the live script — must not. The false-positive half is the half that
matters; without it this guard would have been allowlisted within a week.

**OPEN, recorded rather than silently carried:**

| # | Hole | Why it is not closed here |
|---|---|---|
| 4 | `node`, `npx`, `npm`, `yarn`, `pnpm`, `deno`, `bun`, `ruby`, `gem`, `php`, `lua` are **not banned tokens**. `.mcp.json` uses `npx` live | Banning them would fail the guard on `.mcp.json`, which is dev-session MCP tooling that is never deployed and never in the product path. Removing it breaks local tooling and buys nothing on the box — an **operator call**, not a silent guard edit. `node` is additionally prose-ambiguous (AWS's "SSM managed node") |
| 5 | `.html` is neither banned nor scanned, and the 4-surface frontend carve-out is described in prose but **pinned by nothing** — a 5th `.html` lands green | Needs a budget const mirroring `GITHUB_SCRIPT_BUDGET`, complicated by 12 legitimate vendor-reference `.html` files under `docs/`. Deferred rather than guessed |
| 6 | ~11 GitHub Actions (`actions/checkout`, `actions/cache`, `Swatinem/rust-cache`, …) are `using: node20` JS actions, while `github-script` **is** budgeted as an interpreted surface | The scope is genuinely inconsistent, but the boundary is a policy question: third-party CI actions are not "our workspace codebase". Needs an operator ruling, then either a budget or an explicit written boundary |

Items 4–6 each require an operator decision, so they are stated here instead of
being resolved by executor judgment. None of them is an active violation today.

## §1. The rule (one line)

**Every new executable / runtime component defaults to Rust with O(1) hot-path discipline — the three principles: (1) zero allocation on the hot path, (2) O(1) or fail at compile time, (3) every version pinned — and any non-Rust executable addition needs a fresh dated operator quote recorded in this file FIRST.**

**Scope of "executable / runtime":** lambdas, sidecars, product-path scripts, services, and any process that runs in the product path — these are Rust. Documentation, reference material, protocol notes, and historical audit MAY reference Python (or any language) CONCEPTUALLY — a `.py` snippet in a doc, a vendor SDK cited as a protocol reference, an audit note describing a deleted component — because those are not executable runtime components. This docs-vs-executable boundary is the established interpretation of the phase-1 Rust-Only Purge mission (whose directive was that the tickvault repository be entirely Rust with O(1), now and always); this lock fixes that interpretation permanently so no future session re-litigates it. The single 2026-07-19 quote above is the only dated verbatim authority for this file.

---

## §2. Mechanical enforcement (the teeth — this file is the governance lock, the tests are the teeth)

| Gate | Where | What it enforces |
|---|---|---|
| Phase-3 rust-only ratchet | `crates/common/tests/rust_only_guard.rs` | A **shrinking allowlist** of the last non-Rust executable files — the allowlist may only SHRINK, never GROW. A new non-Rust executable fails the build. |
| Banned-pattern scanner | `.claude/hooks/banned-pattern-scanner.sh` | Blocks banned patterns (hot-path allocation, etc.) in Rust source at commit; a non-Rust executable added under a non-`.py` name is a residual caught in review (§3), not by this scanner. |
| Hot-path discipline | `.claude/rules/project/hot-path.md` | Zero allocation, O(1) constraints, banned hot-path patterns. |
| Exact-version pinning | root `Cargo.toml` workspace deps | `^`/`~`/`*`/`>=` BANNED; `cargo update` BANNED; ONLY exact pins. |

This file is the governance lock; the mechanical teeth are the guard test + scanners that ride the SAME PR — they land together so the rule and its enforcement can never drift apart.

**Honest envelope:** this file cannot, by itself, stop a non-Rust executable from landing — it is the recorded operator intent. The build-failing power lives entirely in the `rust_only_guard.rs` shrinking allowlist and the banned-pattern scanner. If a future PR both adds a non-Rust executable AND grows the allowlist to permit it, only a reviewer honoring §3 catches it: the shrinking allowlist mechanically fails the build on any new tracked Python file that is not already listed, and drops entries as their files are removed — so the automated floor only ever ratchets DOWN; re-growing the allowlist (adding a Python file together with its own allowlist entry) is not mechanically blocked and is caught in review by the §3 reject list, and this file names where the teeth are so no session forgets.

---

## §3. What a PR that violates this lock looks like (REJECT)

- Adds a NEW non-Rust runtime executable (lambda, sidecar, product-path script, service) to the product path.
- GROWS the `rust_only_guard.rs` allowlist (it may only shrink, never grow).
- Removes, softens, or `#[ignore]`s the guard test, or deletes/weakens this rule file.
- Adds a non-Rust runtime dependency to any product-path component.
- Re-introduces a deleted non-Rust component (e.g. a Python sidecar) into the runtime rather than as a reference/doc note.

Any such PR MUST be rejected in review even if the operator approves verbally — the operator must update THIS file FIRST with a fresh dated quote, only then can the PR land.

---

## §4. Trigger (auto-loaded)

Always loaded (this file is under `.claude/rules/project/`). Reinforced on any session that adds a new executable component, edits `crates/common/tests/rust_only_guard.rs`, or proposes any non-Rust runtime addition.

---

> Sir, the shop kitchen speaks ONE language forever — Rust — and every cook works the O(1) way. If anyone wants to sneak in a cook who speaks a different language, the kitchen door stays locked until you sign a fresh dated note on this very board.

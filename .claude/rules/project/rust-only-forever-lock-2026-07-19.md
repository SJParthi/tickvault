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

| # | Hole | Status |
|---|---|---|
| 4 | `node`, `npx`, `npm`, `yarn`, `pnpm`, `deno`, `bun`, `ruby`, `gem`, `php`, `lua` are not banned tokens | **CLOSED 2026-08-15** — see the command-position section above. All eleven are covered; the four non-node runtimes had their FILE extensions banned already, but an extension ban is not an invocation ban (the same distinction the 2026-08-01 `pip` correction turned on), and all four have zero live invocations |
| 5 | `.html` neither banned nor scanned; the 4-surface frontend carve-out was prose pinned by nothing | **CLOSED 2026-08-14 by `browser_surface_and_toolchain_guard.rs`** (landed on `main` via #1753), which pins tracked `.html` as one frontend surface plus vendor docs under `docs/`, AND pins browser code inside `.rs` to the enumerated surfaces. A duplicate budget written in parallel in this file was deleted rather than kept alongside it |
| 6 | ~11 GitHub Actions (`actions/checkout`, `actions/cache`, `Swatinem/rust-cache`, …) are `using: node20` JS actions, while `github-script` **is** budgeted as an interpreted surface | **BOUNDED 2026-08-18** (was STILL OPEN). The boundary question is NOT answered — vendor CI actions are still not "our workspace codebase", and this does not ban them. What changed is that the surface is no longer UNDEFINED: `CI_ACTION_ALLOWLIST` pins the **14** distinct action NAMES actually in use (never versions — tags and SHAs rotate legitimately), so a NEW vendor runtime entering CI fails the build instead of arriving unannounced, and a no-longer-used entry must be removed. Bite-proven both directions. An operator ruling could still ban them outright; until then the count can only shrink |

**2026-08-18 — HONEST LIMIT 2 (the wrapper hole) is CLOSED for the literal form.**
The row above the residuals table recorded that a spawn routed through a wrapper
function was invisible, and named the live example
(`tickvault-logs-mcp/src/tools.rs::run_with_timeout`). `run_with_timeout("` is now
a scan marker alongside `Command::new("` / `.arg("` / `.args([`, so
`run_with_timeout("<interpreter>", ["-c", …])` fails the build — bite-proven
end-to-end against the real file, where it previously passed green. That shape
mattered most because an inline `-c` payload dodges BOTH the file-extension ban
and the shebang fallback, so it was the one form with no backstop at all.
**NOT closed:** a wrapper that is not named in the marker list, and HONEST LIMIT 1
(a spawn whose program is a variable) — the latter is now BOUNDED rather than
fixed: `NON_LITERAL_SPAWN_BUDGET` pins the **6** production sites
(`infra.rs` ×4, `tv_doctor.rs` ×1, `tools.rs` ×1), so a NEW variable-spawn site
fails the build. Resolving a variable still needs call-graph analysis, which a
string scan cannot do, and that residual stays on the record.

Item 6 is the only one left, and it is a policy call rather than an executor
judgment: banning it would mean vendoring or rewriting the standard CI actions
this repo depends on. It is not an active violation — nothing in `crates/`,
`scripts/`, or `deploy/` runs a non-Rust runtime.

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

## §3.1. Progress note (2026-07-19, BATCH-5)

The `tickvault-logs-mcp` cutover parity harness (`crates/tickvault-logs-mcp/tests/parity.rs`) — a
test-time `python3` + git-network fixture that re-materialized the deleted
`scripts/mcp-servers/tickvault-logs/server.py` from pinned git history to prove the Rust port
reproduced the Python original response-for-response — is RETIRED. Its target no longer exists in
the tree, the cutover is done, and the Rust crate is the sole runtime; removing it drops the last
`python3` dependency of that crate's test surface. No allowlist entry changed (parity.rs was Rust).

---

## §4. Trigger (auto-loaded)

Always loaded (this file is under `.claude/rules/project/`). Reinforced on any session that adds a new executable component, edits `crates/common/tests/rust_only_guard.rs`, or proposes any non-Rust runtime addition.

---

> Sir, the shop kitchen speaks ONE language forever — Rust — and every cook works the O(1) way. If anyone wants to sneak in a cook who speaks a different language, the kitchen door stays locked until you sign a fresh dated note on this very board.

## §0.2. 2026-08-20 — SCOPE FIX #8: the dependency graph's own build systems (the SIXTH miss)

A fresh adversarial sweep, told to assume the previous five fixes had missed
something, found the sixth. It is the same shape as all five before it: **the
hole was in what the scan LOOKED AT, and a comment asserted the hazard could
not exist.**

`rust_only_guard.rs` excluded `.lock` files from scanning with the comment
*"Machine-generated dependency graphs; **nothing executes from them**."*

That premise is false in exactly the way the 2026-08-01 `pip` premise was
false. A lockfile does not execute — but it **NAMES crates whose `build.rs`
does**:

| Evidence | |
|---|---|
| `Cargo.lock` | `aws-lc-sys 0.44.0` declares build-deps `cc`, **`cmake`**, `pkg-config` |
| `Cargo.lock` | `cmake 0.1.57` is resolved into the graph |
| Upstream | `aws-lc-sys` drives **CMake** — a separate scripting language — and on some targets **NASM**, to compile the AWS-LC **C** library |
| Reached via | `aws-lc-rs`, the TLS provider CLAUDE.md **mandates** |
| When | every clean `cargo build`, including the `aarch64-unknown-linux-musl` deploy cross-compile |

**This is a BUILD-time surface, not a runtime one — and that distinction is
why it is BOUNDED rather than banned.** Nothing non-Rust runs in production:
all thirteen Lambdas declare `provided.al2023` with a `bootstrap` handler
(verified in `deploy/aws/terraform/*.tf`), and the systemd unit runs
`/opt/tickvault/bin/tickvault`. Banning it would mean dropping the mandated
TLS provider.

**The fix:** `NATIVE_BUILD_TOOLCHAIN_BUDGET` — a shrink-only allowlist, the
same shape as `CI_ACTION_ALLOWLIST` — pinning the three native build systems
actually in the graph (`cc`, `cmake`, `pkg-config`). A NEW one entering (a
vendored C++ library, a Go toolchain, an autotools crate) now fails the build
instead of arriving unannounced in a lockfile nobody reads. The stale half is
enforced too: a budget entry that leaves the graph must be removed in the same
PR, so the ratchet cannot outlive what it bounds.

**Bite-proof, and how it was earned.** The obvious proof — plant a package in
`Cargo.lock` and watch the guard fail — DOES NOT WORK: `cargo test` validates
and rewrites the lockfile before the test binary reads it, so the plant is
gone by the time it matters. Discovered by trying it and getting a green run.
The detection logic is therefore proven against fixtures
(`native_build_toolchain_self_test`), which is stronger anyway because it pins
the parser as well. Recorded because "I planted the exploit and the guard
passed" is a result that could easily have been read as "the guard works".

**Also found — documentation lag, not a hole:** §0.1's scanned-surface list
omits `.plist`, `.xml`, `.alloy`, `.conf`, `.timer`, `.service` and
`.config/nextest.toml`. The guard covers all seven
(`rust_only_guard.rs:251-269`, `:1329`); the PROSE under-states real coverage.
The opposite of the usual failure, and worth saying so.

## §0.3. 2026-08-21 — SCOPE FIX #10: hole SEVEN, and it was the same shape again

Operator directive (2026-08-21, typed directly in-session — preserve EXACTLY,
typos included):

> "Ensure to use one and only RUST O(1) in the entire workspace codebase except frontend alone so check this every nook and corner with assurance and guarantee"

A fresh adversarial sweep, told to assume the previous six fixes had missed
something, found the seventh. **It is the same shape as all six before it**, and
this time it was in the fix that closed hole four.

### The hole

The 2026-08-15 node-family fix (§0.1 item 4) deliberately scanned **command
position** rather than free text, so that `scripts/aws-autopilot.sh`'s three
"SSM managed node" prose lines would not become three false positives. That
reasoning was right and is preserved. The IMPLEMENTATION was not:
`is_command_position` asked whether the text before the token **ended with** one
of nine strings — `|`, `&&`, `||`, `;`, `$(`, `(`, backtick, `"command":`, `"`.

Every one of these ends with none of them, and each was verified to count **0**:

| Form | Where it is the DOMINANT form |
|---|---|
| `run: npx -y pkg` | GitHub Actions single-line step |
| `RUN npm ci` | Dockerfile |
| `ExecStart=/usr/bin/node /opt/app.js` | systemd unit |
| `sudo npm install -g pkg` | provisioning script |
| `exec node app.js` | container entrypoint |
| `env node app.js` | shell wrapper |

`is_command_position` is the **only** detector for eleven runtimes (`node`,
`npx`, `npm`, `yarn`, `pnpm`, `deno`, `bun`, `ruby`, `gem`, `php`, `lua`) — the
python family gets a free-text scan instead. So those eleven were invisible in
CI, Docker and systemd simultaneously, and the self-test's false-negative half
contained none of the six forms, so it proved nothing about them.

**The live tree was and is CLEAN** — zero occurrences of any form. This was a
latent scope hole, not an active violation.

### The fix, and why it is a different KIND of fix

The question changed from *"does the prefix end with a known separator?"* to
**"is the prefix ENTIRELY made of things that precede a command?"**. The second
question has a bounded answer; the first has an endless list, which is precisely
why the list has now been wrong seven times.

`is_command_position` now PARSES the prefix: it splits at the last shell
separator (`=` included, for the systemd form), then repeatedly consumes a YAML
list marker, a key ending in `:`, an assignment, an opening quote, a
[`COMMAND_INTRODUCERS`] word, or a binary path — and returns true only when
nothing is left. `echo` is deliberately NOT an introducer: a printer is how this
check would start reporting sentences.

**SCOPE FIX #10 pins eleven must-count forms and six must-NOT-count forms.** The
false-positive half is the half that matters — a guard whose first act is a
false positive teaches the reader that the cheapest fix is an allowlist, which
is how three anchors in this branch were weakened before. Bite-proven both ways:
deleting one introducer turns `RUN npm ci` red; restoring it turns it green.

### HONEST LIMIT, recorded rather than papered over

An env-var PREFIX (`FOO=bar node app.js`) is **not** covered. After the `=`
split the remainder is `bar`, a bare word, and accepting bare words would make
`managed node` a hit. A miss there is a false negative; accepting it would make
this a false-positive engine, and this guard survives only while its first act
is never a false positive.

### Two findings from the same sweep, NOT fixed here

1. **The browser guard counts `<script` TAGS, not JavaScript** — so §0.1 item 5's
   claim that it "pins browser code inside `.rs`" overstates it. A budget-1
   surface can grow from 20 to 20,000 lines of JS inside one tag with the count
   unchanged, and JS carrying no `<script` (an inline `onclick=`, a route
   serving `application/javascript`) is uncounted entirely. **Verified clean
   today**: zero `application/javascript`, zero `on*=` handlers, `<script` counts
   match the budget exactly across 10 files.
2. **The lockfile check lists native BUILD systems, not embedded INTERPRETERS.**
   A crate like `pyo3`, `mlua`, `rhai`, `deno_core`, `boa_engine`, `rquickjs` or
   `rustpython` ships an interpreter INSIDE the Rust binary: no banned file, no
   banned token, no builder match — green everywhere. **Verified: zero present
   in `Cargo.lock` today.**

Both are recorded as known gaps with a clean current state. Closing them is a
separate change; leaving them undocumented would repeat exactly the mistake this
section exists to record.

## §0.4. 2026-08-29 — SCOPE FIX #11: both §0.3 residuals CLOSED, and the first
## attempt at one of them was itself an inflating guard

Operator directive (2026-08-29, typos preserved):

> "Ensure to use one and only RUST O(1) in the entire workspace codebase except frontend alone so check this every nook and corner with assurance and guarantee"

§0.3 ended with two known gaps and the sentence *"Closing them is a separate
change"*. This is that change. Both were recorded as **"verified clean today"**
— and that phrase is precisely the substitution this file exists to stop: a
measurement carries a date, an enforcement does not.

### Residual 2 — embedded interpreters — was ALREADY CLOSED, and §0.3 was stale

`rust_only_guard.rs::embedded_interpreters_are_absent_from_the_locked_graph`
landed **2026-08-24**, five days before §0.3's own text called it open. It bans
17 names (`boa_engine`, `deno_core`, `duktape`, `hematita`, `mlua`, `neon`,
`pyo3`, `quick-js`, `quickjs-rs`, `rhai`, `rlua`, `rquickjs`, `rustpython-vm`,
`v8`, `wasmer`, `wasmi`, `wasmtime`) from `Cargo.lock`, with an anti-vacuity
floor so a broken parser cannot report green, and a documented carve-out for
`wasm-bindgen`/`js-sys`/`web-sys` (present transitively but declared under
`cfg(target_arch = "wasm32")`, so never compiled for our targets).

Recorded rather than quietly deleted, because it is this file's own recurring
failure arriving again: **a section describing enforcement went stale in the
reassuring→alarming direction**, and the cost is a session spent rebuilding
something that exists. The same lesson the O(1) table records for
`day_ohlc_tracker` (2026-08-12) and `WAL-SUSPEND-01` (2026-08-25).

### Residual 1 — the browser guard — is NOW closed, by three new tests

`browser_surface_and_toolchain_guard.rs` counted `<script` TAGS. Three holes,
all now enforced shrink-only rather than measured:

| Hole | New enforcement | Bite-proven |
|---|---|---|
| One tag can hold 20,000 lines; the count stays 1 | `JS_VOLUME_BUDGET` — exact JS LINE count per surface (board 263, dashboard 235, feeds 226, console.html 339) | +600 lines inside an existing tag → `GREW: … has 835, budget allows 235` |
| JS with no `<script` at all: an inline `onclick=` | `INLINE_HANDLER_BUDGET` — exact count per file (console.html 21, `operator_control.rs` 1, the latter an XSS fixture) over 11 handler attributes, case-insensitive, rejecting `onloaded`/`on_click` substrings | one `onclick=` added to `health.rs` → `UNBUDGETED: … has 1 inline event-handler attribute(s)` |
| A route serving `Content-Type: application/javascript` | `nothing_serves_a_javascript_content_type` — hard ban, not a budget (verified absent, so it stays absent) | the literal added to `health.rs` → `health.rs mentions application/javascript` |

Plus `the_volume_budget_and_the_tag_budget_name_the_same_surfaces`, so the two
frontend tables cannot drift apart, and stale-entry assertions on both budgets
so a ratchet cannot outlive the file it bounds.

### ⚠ The first version of the volume counter INFLATED, and that is worth more than the fix

The line counter originally ran from `<script` to end of file when no closing
tag followed. In Rust source whose `<script` occurrences are XSS fixtures, that
reported **2,320 lines of "JavaScript" in `notification/events.rs`** and 1,896
in `operator_control.rs`. A second attempt paired an opener at line 5,960 with
an unrelated fixture closer at line 7,094 — 1,137 lines of nonsense.

An inflating guard is not the safe direction. It is abandoned or re-baselined
as fast as one that under-reports, and the re-baseline is what actually removes
the enforcement. Two fixes: an unterminated opener now contributes **its own
line and nothing more**, and the volume scan is **scoped to the carve-out
itself** — the enumerated frontend surfaces plus every tracked `.html`. It does
not attempt to measure "lines of JavaScript" in arbitrary Rust files, because
their `<script` occurrences are string literals and any line-pairing over them
is meaningless. Those files remain guarded EXACTLY, by tag count, in
`SCRIPT_BUDGET`.

### What is now enforced vs measured

| Question | Before | After |
|---|---|---|
| Can a new browser surface appear? | enforced (`SCRIPT_BUDGET`) | unchanged |
| Can an existing surface's JS grow without limit? | **no enforcement** | enforced, exact, shrink-only |
| Can JS arrive with no `<script` tag? | **invisible** | enforced (handlers + media type) |
| Can an embedded interpreter enter the graph? | enforced since 2026-08-24 | unchanged (§0.3 was stale) |

**Still not claimed.** `HANDLER_ATTRS` is a list of 11 attributes, and no list
of DOM events can be exhaustive — a page using `onpointerdown` would pass. The
volume budget counts LINES, not semantics: 235 lines of minified JavaScript is
a great deal more code than 235 lines of formatted JavaScript, and nothing here
measures that. Both are bounded by the same shrink-only discipline as every
other budget in this family, and both are stated so the next reader knows where
the edge is rather than discovering it.

## §0.5. 2026-09-01 — SCOPE FIX #14: the interpreter ban enumerated ONE MEMBER PER FAMILY

Operator directive (2026-09-01, typos preserved):

> "Ensure to use one and only RUST O(1) in the entire workspace codebase except frontend alone so check this every nook and corner with assurance and guarantee"

§0.4 closed the two residuals §0.3 had left open and reported the
embedded-interpreter check as settled. It was not. The check was real and it
ran — but it matched package names by **exact equality**, so it banned exactly
one member of each interpreter family and was blind to every sibling.

### The hole

`EMBEDDED_INTERPRETERS` listed `rustpython-vm`. RustPython actually reaches a
graph as any of `rustpython-vm`, `rustpython-compiler`, `rustpython-parser`,
`rustpython-stdlib` or `rustpython-common`, depending on which crate the
dependent names — and only the first was banned. The same shape applied to
every multi-crate runtime in the list.

Eight families had **no entry at all**: `rune`, `starlark`, `extism`, `wasm3`,
`gluon`, `koto`, `steel-core`, and `rustpython` as a family rather than one
crate. Each embeds a language runtime **inside** the Rust binary, where no
file-extension ban, no shebang check and no spawn scan can see it.

This is the seventh instance of the identical failure §0.3 names: **an
enumeration of members is wrong by construction, and a class nobody listed
reads as green.**

### The fix

`is_in_crate_family(name, family)` replaces `name == family`. A crate is in a
family when it IS the family or starts with `"{family}-"`. So one `rustpython`
entry now covers all five sibling crates, and the list widened 17 → 24
families.

**The hyphen is load-bearing and is the half that keeps this honest.** A bare
prefix would make `rune` fire on `runes` and `v8` on `v8x`. A guard whose
first act is a false positive teaches the next reader that the cheapest fix is
an allowlist entry — which is how three anchors in this branch were weakened
before.

**Verified before widening, not after:** none of the 24 families matches any
of the 476 packages across the tracked lockfiles. This adds enforcement
without adding a single exemption.

**Bite-proven both directions** (`embedded_interpreter_detection_self_test`):
reverting `is_in_crate_family` to exact equality fails the sibling assertion
(1 failed), and restoring it passes the full suite (23 passed). The
false-positive half is pinned by three explicit near-miss assertions
(`runes`/`rune`, `rustpythonic`/`rustpython`, `v8x`/`v8`).

### Deliberately NOT added

`wasm` as a family. It would match `wasm-bindgen`, which IS in the lockfile
today — pulled transitively by chrono, getrandom, uuid, reqwest and
opentelemetry, every one of them under `cfg(target_arch = "wasm32")`, so never
compiled for a target we ship. Banning it would fail the build over packages
that exist in no artifact we produce. `wasm3` is the specific interpreter;
`wasm` is not. That carve-out is unchanged from §0.4 and is re-verified here.

### Still not claimed

A family list is still a LIST. It is now a list of families rather than a list
of crates, which is one level less wrong — but a runtime whose crate name
shares no prefix with anything here still passes. What changed is that adding
the next one costs a single entry instead of one entry per sibling, and that
the seven names added today are enforced rather than assumed absent.

## §0.6. 2026-09-01 — SCOPE FIX #15: hole EIGHT, and it was TWO holes, both CRITICAL

Same operator directive as §0.5, same day. A hunt commissioned specifically to
find "hole eight" — told to assume it existed — found two, and the live tree is
CLEAN of both, so these close LATENT breaches rather than active ones.

### Hole 8a — a shebang was checked for EXISTENCE, never for WHAT IT NAMES

`has_interpreter_shebang` asks whether a first line starts with `#!`. That is
the right question for deciding **which files to scan** (SCOPE FIX #8's whole
point) and the wrong one for deciding **what is allowed**.

A tracked, extension-less, executable file whose first line reads
`#!/usr/bin/env node` cleared every check in `rust_only_guard.rs`
**simultaneously**:

| check | why it missed |
|---|---|
| `BANNED_FILE_PATHSPECS` | no extension to ban |
| `banned_tokens()` | `node` is not in the python family |
| `count_node_invocations` | `is_command_position` sees a prefix starting with `#` and matches no arm |
| `every_tracked_executable_is_inside_the_invocation_scan` | the file IS in the scan — it **passed** |

That last row is the sharp one: the existing test proves a shebang file is
*being scanned*, which buys nothing when the scan cannot read the line. A
guard that is satisfied by the hostile artifact is worse than no guard.

**Fixed** by `shebang_runtime()`, which resolves what the line actually names
(`env` and `-S` forms included, so `#!/usr/bin/env -S deno run --allow-net`
resolves to `deno`), and `every_tracked_shebang_names_an_allowed_runtime`,
which permits **`bash` and `sh` only**. All ~99 tracked shebangs are `bash`, so
this lands at a hard floor with **no allowlist to grow**.

### Hole 8b — `NODE_FAMILY` was never applied to Rust at all

`no_rust_spawn_of_banned_interpreter` filtered spawn literals through
`banned_tokens()` alone — the python family plus perl. The node family was
checked only by `node_family_invocations_only_shrink`, which scans
`load_invocation_scan_files()`, and that **excludes `.rs`**. So
`Command::new("node")` in Rust passed both, each believing the other covered
it.

The sibling browser guard did not help: its `SPAWN_ALLOWLIST` reads only
`Command::new("`, so `Command::new("bash").arg("-c").arg("node /opt/x.js")`
shows it the allowlisted `bash` and nothing else.

**Fixed** by `spawn_literal_names_node_family`, which catches both shapes — the
literal that IS the runtime (`"node"`, `"/usr/bin/node"`) and the literal that
CARRIES it in command position (the `-c` payload above). Reusing
`count_node_invocations` for the second shape is deliberate: it already
encodes the command-position parser **and** its false-positive discipline, so
`"SSM managed node"` stays a sentence.

### Both bite-proven, against the hunt's own hostile artifacts

| planted | result |
|---|---|
| `tools/tv-report`, extension-less, `#!/usr/bin/env node` | **FAILS** — `tools/tv-report: #! names \`node\`` |
| `Command::new("bash").arg("-c").arg("node /opt/x.js")` in a real `.rs` | **FAILS** — ``spawns `node /opt/x.js` `` |
| both removed | 25 tests pass |

The second proof is the one worth keeping: the program was the **allowlisted**
`bash`, and the runtime hidden in an argument was still caught.

### One exemption added, and why it is two names rather than a glob

Extending the spawn scan to `NODE_FAMILY` matched `Command::new("node")` inside
`browser_surface_and_toolchain_guard.rs` —
`spawn_scanner_extracts_literals_and_ignores_non_literals`, a raw-string
FIXTURE that bite-proves that guard's own extractor. A guard cannot prove it
detects a thing without writing the thing down, which is exactly why
`rust_only_guard.rs` has always skipped itself.

`SELF_REFERENTIAL_GUARDS` is an explicit **two-file list**, never a
`*_guard.rs` glob. A glob would silently exempt every future guard file, and
that is precisely how an exemption becomes a hole. A third entry is a visible
diff and needs the same justification.

### Still open, recorded rather than quietly carried

The same hunt found six more, all latent and all with the live tree CLEAN.
None is fixed here and none should be assumed covered:

| # | Hole | Sev |
|---|---|---|
| H10 | argv **arrays** — `"args": ["node","app.js"]`, terraform `command = ["node"]`, compose `command: [node, s.js]`. `[` is not a separator in `is_command_position`. This is the shape `.mcp.json` itself uses | HIGH |
| H11 | bare YAML sequence item `  - node` (the quoted form `- "node"` IS caught; `before` is `trim_end`'d so the `- ` arm never fires) | HIGH |
| H12 | unlisted wrapper binaries — `find … -exec node`, `watch`, `setsid`, `stdbuf`, `parallel`, `ssh box "node …"`, and make's `@`/`-` recipe prefixes. `COMMAND_INTRODUCERS` enumerates 17 names, which is the enumerate-names failure this file keeps recording | HIGH |
| H13 | `.wasm` is excluded from every guard at once; `.wat` is neither banned nor token-detectable | MED |
| H14 | the browser guard opens only `*.rs` and `*.html` — `<script>` in a tracked `.svg`, `.md`, `.json` or a `.tftpl` is uncounted by all three budgets | MED |
| H15 | `.md` is excluded outright, so a new `SKILL.md` with fenced interpreter code is invisible to both guards — the exact class deleted on 2026-07-31 | MED |
| H16 | `cargo_config_declares_no_external_runner_or_linker` is root-only and **fails open** on an unreadable file; a per-package `crates/app/.cargo/config.toml` is never opened | MED |

H12 is the one that matters most conceptually: it is the same enumeration
failure as 8a, one level out. Closing it properly means asking what a token is
in the LINE's grammar rather than listing the words that may precede it — the
same move made twice now, and not yet made a third time.

## §0.7. 2026-09-01 — SCOPE FIX #16: four of the seven open holes CLOSED

Same operator directive as §0.5/§0.6, same day. The §0.6 table above says
"None is fixed here" — this section fixes **H10, H11, H12 (partly) and H16**.
The live tree was and remains CLEAN of all four, so these close LATENT holes.

### H10 — argv arrays (HIGH)

`[` was not a separator, so an argv array put the runtime in command position
with no shell separator anywhere on the line and every form read as a mention:

| form | where it is the dominant shape |
|---|---|
| `"args": ["node", "app.js"]` | **`.mcp.json` in this repo** |
| `command = ["node", "server.js"]` | terraform |
| `command: [node, server.js]` | docker-compose |

The file that motivated the node-family ban was itself written in a shape the
ban could not read.

**Fixed** by adding `[` to the separator set. `,` is a separator too, but
**only when an open bracket appears earlier in the line** — a general comma
separator would put the runtime word in command position for any prose
containing `", node"` and fail the build on a sentence. Six prose fixtures
pin that (`"restarts the box, node counts stay flat"` and five siblings).

### H11 — bare YAML sequence item (HIGH)

`  - node`. The `- ` arm could never fire on it: `before` is `trim_end()`'d,
so the trailing space the arm needs is already gone by the time it runs. Only
the QUOTED form `- "node"` was caught, which is the rarer style.

**Fixed** by a whole-segment marker arm.

### H12 — wrapper prefixes (HIGH) — PARTLY closed, and the residual is stated

**Closed:** make recipe prefixes (`@`, `-`, `@-`, `-@`, `+`) as whole-segment
markers; `-exec` as a separator, which covers `find . -exec node`; and
`watch`, `setsid`, `stdbuf`, `parallel`, `nice`, `ionice`, `doas` added as
introducers.

**NOT closed, deliberately:** `ssh host "node app.js"`. It places a BARE WORD
(`host`) between the wrapper and the runtime, and the parser must consume the
ENTIRE prefix to return true. Accepting bare words is exactly what would turn
`SSM managed node` into a build failure — the false positive this guard cannot
survive. Recorded at the site as `_WRAPPER_SHAPES_NOT_COVERED` rather than left
to be rediscovered. A miss here is a false NEGATIVE; the alternative is a
false-positive engine, and a guard whose first act is a false positive gets
allowlisted within a week.

### H16 — cargo config: root-only AND fails open (MED)

Two defects in one test. It read only `.cargo/config.toml` at the repo root —
but cargo reads the file from the package directory and every ancestor, so
`crates/app/.cargo/config.toml` sets the runner/linker for that package and was
never opened. And `let Ok(body) = … else { return }` meant an unreadable config
**passed as trivially safe** — the one case that most needs to fail was the one
case waved through.

**Fixed:** enumerates every `.cargo/config.toml` and `.cargo/config` via
`scan_paths` (tracked AND untracked, the SCOPE FIX C1 lesson), and panics on an
unreadable one. This matters because §0's own record has an interpreter package
ACTUALLY being the arm64 linker of every production lambda while reading green.

### Bite-proofs (both directions, planted then removed)

| planted | before | after |
|---|---|---|
| `scripts/planted-argv.json` with `"args": ["-c", "node /opt/evil.js"]` | **passes green** | **FAILS** — `("scripts/planted-argv.json", 1, 0)` |
| `crates/app/.cargo/config.toml` with `runner = "node-emulator"` | never opened | **FAILS** — names the per-package path |

The first was verified by reverting the parser to HEAD with the plant still in
the tree and watching it pass — the fix, not the fixture, is what catches it.

### Still open after this section

**H13** (`.wasm`/`.wat`), **H14** (the browser guard opens only `*.rs` and
`*.html`), **H15** (`.md` excluded, so a `SKILL.md` with fenced interpreter
code is invisible) and the `ssh` half of H12. H15 is the awkward one: scanning
`.md` naively would flag this repository's own rule files, which discuss the
banned runtimes at length — the honest shape is to scan only FENCED CODE BLOCKS
carrying an interpreter language tag, which is a parser, not a predicate.

## §0.8. 2026-09-02 — SCOPE FIX #17: four holes the second sweep found, and the first attempt at one was a false-positive engine

Operator directive (2026-09-02, given in direct response to the Second Sweep
Ledger, whose finding-14 row named these holes — typos preserved):

> "go ahead and fix the remaining open findings dude okay?"

> "Once fixed finished and resolved merge and deploy it also dude okay?"

Same shape as §0.5–§0.7: the live tree was and remains CLEAN of all four, so
these close LATENT holes, and every one is bite-proven in both directions
because the real-tree tests cannot demonstrate that anything is caught.

### H-a — a non-UTF-8 file was SKIPPED, silently, by nine scans

`read_to_string` fails on ONE invalid byte. Eight sites in
`rust_only_guard.rs` and five in `browser_surface_and_toolchain_guard.rs`
read with `.ok()?`, `let Ok(..) else { continue }` or `unwrap_or_default()`
— so a Latin-1 `é` in a comment took the whole file out of every scan at
once, and a file with `\xff\xfe` on line 1 and `node app.js` on line 2 was
invisible. The comment at the loader said this was deliberate ("a guard that
crashes on one is a guard someone disables"), which is the reassuring-comment
class §0.2 records.

**Fixed:** `decode_scan_bytes` / `read_scan_text` decode LOSSILY (invalid
bytes become U+FFFD, which matches nothing; every valid byte around them is
still scanned) and PANIC on an I/O failure — a listed file the guard cannot
open is a guard failure, never a file to skip. All thirteen sites use them.

### H-b — a `bash -c` shebang hands bash a program nobody scanned

`#!/usr/bin/env -S bash -c "node app.js"` names an ALLOWED runtime, so
§0.6's `shebang_runtime` waves it through, and the payload is invisible to
every line scanner: the line starts with `#!`, `is_command_position`
matches no arm on that prefix, and the `node` is never counted. The kernel
runs exactly that `node`.

**Fixed:** `shebang_inline_payload` extracts the program after a SHORT flag
cluster containing `c` (`-c`, `-ec`, `-ce`) and feeds it to both
`count_node_invocations` and the interpreter-token scan. `-euo pipefail`
(no `c`), `--norc` (long option) and a bare `-c` yield no payload — pinned,
because most of this repo's own scripts open with `-euo pipefail`.

### H-c — every scan enumerated TRACKED files only

A new interpreter script, a `.go` source, a shebang wrapper — none appears
in `git ls-files` until `git add`, so the guard reported green on exactly
the change it exists to catch: the first commit of a new runtime. The
browser guard has carried `--others --exclude-standard` since its own C1
bite-test; this file's §0 records an untracked `crates/x/src/evil.rs` being
invisible to every diff source. The lesson was learned and not applied here.

**Fixed:** `git_ls_files_with(extra_args, pathspecs)` and
`git_ls_files_including_untracked`; the invocation scan, both shebang tests,
`no_banned_files_outside_allowlist` and `no_rust_spawn_of_banned_interpreter`
now include untracked-but-not-ignored files. **Deliberately left
tracked-only, documented at the site:** the stale-entry checks and the
shrink-only budgets — a budget entry must name a file that is actually in the
repository, and an untracked scratch file must never satisfy "still tracked".

### H-d — the ban enumerated SCRIPTING languages and stopped

`java`, `go`, `dotnet`, `swift`, `kotlin`, `scala`, `groovy`, `julia`,
`Rscript` — a whole second toolchain in one `RUN go build` line, and none
was a banned token or a banned extension. `*.java .kt .kts .scala .go .cs
.swift .R .r` joined `BANNED_FILE_PATHSPECS` (all nine verified ZERO tracked)
and the ten runtimes joined `NODE_FAMILY`, because an extension ban is not an
invocation ban (the 2026-08-01 `pip` lesson, again).

**Deliberately excluded, with the reason in the docblock and pinned by
test:** `jq` runs the All Green verdict in `ci.yml` (merge-gate-lock §5.1) —
banning it fails the merge gate on its own implementation; `awk`/`sed` are
POSIX text tools every script here uses; `perl` already lives in
`banned_tokens()`; bare `R` is also `chmod -R`.

### ⚠ The first version counted a SENTENCE, and that is the reusable part

A line that begins with a word is command position by definition. `go` and
`swift` are English words. The first version of this fix counted **"swift
recovery is expected after a reconnect"** — a real comment — as an
invocation. That is precisely the false positive this file says a guard
cannot survive: it would have been allowlisted within the week.

**Fixed** by `PROSE_AMBIGUOUS_RUNTIMES` (`go`, `swift`): those two
additionally require a toolchain-shaped NEXT token — a flag, a path or source
file (`main.go`, never a sentence-ending `go.`), or the compiler's own
subcommand (`build`, `run`, `test`, `mod`, …). A word-START boundary was
added to `count_node_invocations` at the same time (`go` inside `cargo`,
`Rscript` inside `myRscript`), with `-` allowed on the left because make's
`-node` recipe prefix is a real invocation — the existing `guard_self_test`
caught the first draft rejecting it.

The must-NOT-count fixtures include the real `sudo rm -rf /usr/share/dotnet`
line from `ci.yml`, `name = "governor"` (the only `Cargo.lock` package with a
family member as a substring), `java-properties`, a `/java/` URL, `cargo
build` under `sudo` and `RUN`, `chmod -R`, and `go to the next step`.

### Bite-proofs (untracked plants, never `git add`ed, then removed)

| planted | result |
|---|---|
| `scripts/planted-shebang-wrapper` — `#!/usr/bin/env -S bash -c "node /opt/evil.js"` | **FAILS** `node_family_invocations_only_shrink` — `("scripts/planted-shebang-wrapper", 1, 0)` |
| `scripts/planted.go` | **FAILS** `no_banned_files_outside_allowlist` — `["scripts/planted.go"]` |
| `scripts/planted-latin1.sh` — `\xff\xfe` on line 1, `node app.js` on line 2 | **FAILS** — `("scripts/planted-latin1.sh", 1, 0)` |
| `scripts/planted-toolchain.sh` — `RUN go build ./...` + `java -jar x.jar` | **FAILS** — `("scripts/planted-toolchain.sh", 2, 0)` |
| `scripts/benign-planted.sh` — the five must-not-count lines above | **passes**, 26/26 |

All four hostile plants were planted together and every one was named in
the failure output; the benign plant passed with the fix in place.

### Still open after this section

- **H12's `ssh` half** — `ssh host "node …"` still needs a bare word
  consumed, which is the false-positive engine §0.7 refused.
- **Split-argv `"command": "go"`** — a bare ambiguous runtime with nothing
  after it on the line counts 0 by construction (pinned: the same line with
  `node` counts 1). The word IS the sentence; there is nothing to check.
- **H13 / H14 / H15** — unchanged from §0.7.
- The `PROSE_AMBIGUOUS_RUNTIMES` subcommand list is an enumeration, the
  failure shape this file keeps recording. It is bounded by the two
  compilers' own verb sets rather than by what a script author might write,
  which is the narrower and therefore safer direction to be wrong in.

## §0.9. 2026-09-05 — SCOPE FIX #19: hole TEN, and it was inside the fix for hole nine

Same operator directive as §0.5–§0.8. An adversarial sweep was told to read the
guard first, understand exactly what it scans, and find a way a non-Rust
runtime could still enter. It found one, and it was **three days old**.

### The hole

§0.8's SCOPE FIX #18 closed managed Lambda runtimes — the first check in this
file able to see `runtime = "nodejs20.x"` at all. It enumerated the terraform
files with `std::fs::read_dir`, which is **not recursive**.

Every `.tf` in this repository is flat in `deploy/aws/terraform/`, so the check
was correct on the tree it was written against. But the next structural step
any terraform codebase takes is a `modules/` subdirectory, and the first one
would have silently dropped every Lambda inside it out of the **only** check
that can see a managed runtime.

**Nothing else covers it.** The general token scanner is *provably* blind here:
`node` inside `nodejs20.x` is followed by a digit, which fails
`is_command_position`'s word-boundary test. The guard's own panic message says
so. Bite-tested: a planted `modules/planted/main.tf` declaring
`runtime = "nodejs20.x"` left `node_family_invocations_only_shrink` **GREEN**
while the recursive version of the Lambda check FAILED naming the file.

### The fix

`git_ls_files_including_untracked(&["deploy/aws/**/*.tf"])` — recursive, and
untracked-inclusive per SCOPE FIX #17, so a `.tf` that has not been `git add`ed
is scanned too. That closes both halves at once: the first commit of a new
module is exactly when this matters.

### The pattern, now ten for ten

| # | hole | what the scanner did not LOOK AT |
|---|---|---|
| 1–8 | §0.1–§0.4 | extensions, shebangs, spawn forms, cargo config, lockfiles, embedded interpreters |
| 9 | §0.5–§0.8 | argv arrays, YAML sequences, wrapper prefixes, per-package cargo config, non-UTF-8, untracked files, toolchain languages, Lambda runtimes |
| **10** | **this** | **a subdirectory** |

**Ten holes, ten times the same shape**, and this one is the sharpest statement
of it: the hole was not merely in what the scanner looked at, it was in what
the *newest fix* looked at — introduced by the change that closed the previous
hole, three days earlier. A guard's own scope is as much a claim as its word
list, and it goes stale the same way.

### A second finding, recorded and NOT fixed — it is an operator decision

Container images are unbounded on **identity**. `docker_image_digest_guard`
checks that images are digest-PINNED, never what they are; `ci_action_names`
explicitly skips `docker://`; and no `IMAGE_ALLOWLIST` exists anywhere. The
tree already runs a JVM (`questdb/questdb`) and two Go services
(`grafana/loki`, `grafana/alloy`) beside the Rust process.

Vendor CI action **names** were bounded by `CI_ACTION_ALLOWLIST` on exactly the
argument that a new vendor runtime should fail the build rather than arrive
unannounced. That argument applies verbatim to images. A shrink-only
`CONTAINER_IMAGE_ALLOWLIST` would close it at the same cost.

**Not taken here** because whether third-party service images are in scope at
all is a scope question, not an executor's: the operator's directive is about
the workspace codebase, and QuestDB is a database he chose, not code we wrote.
Recorded so the next session decides deliberately rather than rediscovering it.

### What a PR that violates §0.9 looks like (REJECT)

- Reverts the Lambda scan to `read_dir`, or any non-recursive enumeration.
- Drops the untracked-inclusive listing (the first commit of a module is the
  case that matters).
- Exempts a Lambda from the runtime check instead of building its handler in
  Rust.
- Claims the general token scanner covers managed runtimes — it cannot, by
  word boundary, and the bite-test above is the evidence.

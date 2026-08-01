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

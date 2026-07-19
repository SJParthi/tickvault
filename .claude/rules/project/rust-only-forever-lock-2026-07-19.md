# Rust-O(1)-Forever — Operator Lock 2026-07-19

> **Authority:** CLAUDE.md > `operator-charter-forever.md` > `hot-path.md` > this file > defaults.
> **Scope:** PERMANENT. Every environment, every PR, every task, every current and future Claude/Cowork session.
> **Operator-locked:** 2026-07-19 (verbatim demand below).
> **Companion enforcement:** `crates/common/tests/rust_only_guard.rs` (the phase-3 shrinking-allowlist ratchet this lock rides with).
> **Auto-load trigger:** Always loaded (path is in `.claude/rules/project/`).

---

## §0. The verbatim operator demand (preserve exactly, do not paraphrase — typos included)

> "what else is remianing is our entire system became entirley rust dude not only now even in the future whenever it ry to provid enay requirmenets discussiosn or it could be anyhtig dude whatevr it is by default it needs to ebcome RUST O(1) dude okay? can that happen dude?"

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

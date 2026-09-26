# Rust-O(1)-Forever — Operator Lock 2026-07-19 — SUMMARY STUB

> **Full text:** `docs/claude-rules-full/project/rust-only-forever-lock-2026-07-19.md` (moved verbatim 2026-09-26 to keep the auto-loaded context small). **Read the full file before adding or changing ANY non-Rust file, script, workflow step, hook, build system, toolchain file or dependency, or before touching `crates/common/tests/rust_only_guard.rs`.** It holds the verbatim operator quotes, the 2026-08-14 audit and the dated SCOPE FIX sections (§0.2–§0.9) that close each guard hole. Where this summary and the full file differ, the full file wins. Amend the full file first, then keep this summary in sync.

**Authority:** CLAUDE.md > `operator-charter-forever.md` > `hot-path.md` > this file > defaults. **Scope:** PERMANENT. **Companion enforcement:** `crates/common/tests/rust_only_guard.rs`.

**Operator quote 1 (2026-07-19):** "what else is remianing is our entire system became entirley rust dude not only now even in the future whenever it ry to provid enay requirmenets discussiosn or it could be anyhtig dude whatevr it is by default it needs to ebcome RUST O(1) dude okay? can that happen dude?"

**Operator quote 2 (2026-07-31):** "ensure to use only RUST O(1) entolrwy everywhere bro I mean entire workspace codebase everything entirely bro okay? I need the gauarntee and assurnace bro see nowhere the word python shoudl be available dude okay?" — effect: every tracked `.py` file was deleted; the tree is at ZERO.

**The rule (§1, verbatim):** **Every new executable / runtime component defaults to Rust with O(1) hot-path discipline — the three principles: (1) zero allocation on the hot path, (2) O(1) or fail at compile time, (3) every version pinned — and any non-Rust executable addition needs a fresh dated operator quote recorded in this file FIRST.** Scope of "executable / runtime": lambdas, sidecars, product-path scripts, services, any process in the product path. Docs/reference/audit MAY mention other languages conceptually.

**Teeth (§2):** the shrinking-allowlist ratchet in `rust_only_guard.rs` ("may only SHRINK, never GROW"); `banned-pattern-scanner.sh`; `hot-path.md`; exact-version pinning in root `Cargo.toml` (`^`/`~`/`*`/`>=` BANNED; `cargo update` BANNED).

**REJECT (§3, verbatim):**
- Adds a NEW non-Rust runtime executable (lambda, sidecar, product-path script, service) to the product path.
- GROWS the `rust_only_guard.rs` allowlist (it may only shrink, never grow).
- Removes, softens, or `#[ignore]`s the guard test, or deletes/weakens this rule file.
- Adds a non-Rust runtime dependency to any product-path component.
- Re-introduces a deleted non-Rust component (e.g. a Python sidecar) into the runtime rather than as a reference/doc note.

The dated §0.x SCOPE FIX sections each add further REJECT rows for specific guard holes (interpreter invocations, build systems in the dependency graph, etc.) — read them before touching the guard.

"Any such PR MUST be rejected in review even if the operator approves verbally — the operator must update THIS file FIRST with a fresh dated quote, only then can the PR land."

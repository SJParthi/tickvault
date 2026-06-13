# Implementation Plan: Drop 9 dead workspace dependencies (deletion-audit Phase C, evidence-refreshed)

**Status:** APPROVED
**Date:** 2026-06-13
**Approved by:** Parthiban — standing directive 2026-06-13 ("Scope deletion-audit Phase B" + "go ahead with the plan always, make it ready for merge, monitor until merged"). This is the one Phase B/C item that re-verified as genuinely safe against the current tree (the rest of the 2026-05-25 audit is stale/superseded/operator-blocked — see `active-plan-deletion-audit.md` Phase B execution log + the 2026-06-13 3-agent re-verification).

## Design

Remove 9 workspace dependencies that have **zero production usage** in the current tree, confirmed by a fresh 2026-06-13 grep sweep (including attribute-macro false-negative checks for `enum_dispatch`/`statig`/`yata`):

| Dep | Root decl line | Crate-level `{workspace=true}` consumer |
|---|---|---|
| `dashmap` | Cargo.toml:33 | none |
| `hdrhistogram` | Cargo.toml:58 | none |
| `bitcode` | Cargo.toml:118 | none |
| `failsafe` | Cargo.toml:90 | crates/core/Cargo.toml:36 |
| `backon` | Cargo.toml:91 | crates/core/Cargo.toml:28 |
| `yata` | Cargo.toml:92 | none |
| `statig` | Cargo.toml:87 | none |
| `jaeckel` | Cargo.toml:88 | crates/trading/Cargo.toml:25 |
| `enum_dispatch` | Cargo.toml:29 | none |

Files edited: root `Cargo.toml` (remove 9 `[workspace.dependencies]` lines), `crates/core/Cargo.toml` (remove `failsafe`, `backon`), `crates/trading/Cargo.toml` (remove `jaeckel`). **No `crates/*/src/*.rs` touched** → design-first-wall exempt. Cargo.lock shrinks on next build (this is removal, NOT `cargo update` — the ban is not triggered).

NOT dropped (audit called them dead, but the 2026-06-13 agent found them LIVE): `metrics-exporter-prometheus`, `opentelemetry*`, `tracing-opentelemetry`, `csv`, `notify`, `governor`. `signal-hook` intentionally retained (documented Windows-future, machete-ignored).

## Edge Cases

- Attribute-macro crates (`enum_dispatch`, `statig`) used via `#[...]` not `crate::` — verified zero hits incl. comments-only for `statig`. `yata` (TA lib) zero hits.
- Crate-level orphan entries: removing a root `[workspace.dependencies]` line while a crate still lists `dep = { workspace = true }` makes cargo error — both ends removed in the same commit for the 3 affected deps.
- `bitcode`/`rkyv` coexist; only `bitcode` is unused — `rkyv` stays.

## Failure Modes

- **Hidden consumer (build break):** `cargo check --workspace --all-targets` must stay green after removal — that IS the proof there is no consumer.
- **cargo deny advisory regression:** removal only shrinks the tree; `cargo deny check` (if it runs in CI) can only improve.
- **Concurrent main movement:** fetch + verify `origin/main` before push; the #1121 merge already landed.

## Test Plan

- `cargo check --workspace --all-targets` → must be exit 0 (real output pasted in PR).
- `cargo tree -i <dep>` for each → must report "package not found" (no reverse-deps) — paste evidence.
- Pre-push gates (fmt, banned-pattern, secret, pub-fn, plan-verify) green.
- No unit-test run needed (no code change) per `testing-scope.md` (config/manifest-only).

## Rollback

Each removed dep is a one-line revert in the manifest; `git revert <sha>` restores verbatim. Audit classifies Phase C dep removal as "trivially revertable". Zero runtime behavior change.

## Observability

No new code paths, counters, or events. The only observable effect is a smaller dependency tree + faster cold build + smaller binary. No telemetry change required.

## Plan Items

- [ ] Remove 9 deps from root `[workspace.dependencies]`
  - Files: Cargo.toml
- [ ] Remove orphan crate-level entries (failsafe, backon, jaeckel)
  - Files: crates/core/Cargo.toml, crates/trading/Cargo.toml
- [ ] Verify `cargo check --workspace --all-targets` green + `cargo tree -i` shows no reverse-deps
  - Files: (verification only)

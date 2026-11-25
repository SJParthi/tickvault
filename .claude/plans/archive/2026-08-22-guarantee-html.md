# Implementation Plan: generate the guarantee comparison page instead of hand-writing it

**Status:** APPROVED
**Date:** 2026-08-22
**Approved by:** Parthiban (operator), 2026-08-22, third verbatim repeat of the
standing demand — which asks for an "automated easy table level comparison view".
The measurements were already automated (`make guarantees`); the PAGE was not.
It was hand-authored with the numbers baked in, so it would read green while the
code moved underneath it — the stale-document failure this repo has recorded
against its own O(1) table more than once.

**Guarantee matrices:** carried by cross-reference to
`.claude/rules/project/per-wave-guarantee-matrix.md` (15-row + 7-row).

## Design

`tv_guarantees.rs` already models every measurement as a `Row { what, verdict,
measured, proof }` and renders it as fixed-width text. Add a second renderer over
the identical values:

- `HTML_HEAD` — inlined tokens/type/layout; theme-aware for all three viewer states
- `render_html(sections)` — the same `Row`s as a table page, verdict as a chip
- `esc()` — escape `& < > "` so a proof string containing `<` cannot swallow a row
- `--html` in `main`, branching BEFORE the text report; same non-zero exit on Broken
- `make guarantees-html`

One measurement pass, two renderings. The table and the page cannot disagree,
because they are the same data.

## Edge Cases

- A proof string containing `<` or `&` (one exists: the `<=7000 blk/10k` row) —
  escaped, verified by counting `&lt;` in the output.
- Zero rows in a section — renders an empty tbody, not malformed HTML.
- `Broken > 0` must still exit non-zero in HTML mode, or CI would treat a broken
  guarantee as a successful page render.

## Failure Modes

- Unbalanced tags -> silently truncated page. Verified by counting open/close
  pairs for section, table, tr, td.
- Divergence between text and HTML -> impossible by construction (same `Row`
  slice), and cross-checked: tiles read 19/10/2/0, text summary reads the same.
- CSS defined only inside a media query -> unreadable page in the un-stamped
  theme state. Tokens are defined on bare `:root` first.

## Test Plan

- `cargo build -p tickvault-app --bin tv-guarantees`
- `cargo run -p tickvault-app --bin tv-guarantees` (text unchanged)
- `cargo run -p tickvault-app --bin tv-guarantees -- --html` (page emitted)
- tag-balance count on the emitted page
- tile counts must equal the text summary line

## Rollback

`git revert` of the single commit. The text report is untouched by design, so a
revert cannot affect the CI gate that consumes it.

## Observability

No new counters or alarms — this is a reporting surface, not runtime behaviour.
The observable outcome is that `make guarantees-html` emits a page whose numbers
match `make guarantees` on the same tree.

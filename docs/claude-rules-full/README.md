# Full text of large always-loaded rule files

Claude Code auto-loads every `.claude/rules/**/*.md` file that has no `paths:`
frontmatter into every session's starting context. By 2026-09-26 that set had
grown to about 1.57 MB of rules plus the 125 KB root `CLAUDE.md`. That was
enough to overflow sessions at startup.

Every always-loaded rule file over 15 KB was moved here **verbatim** (`git mv`,
so its history is kept), under the same relative path it had under
`.claude/rules/`. A short summary stub stays at the original path. The stub
holds the binding rules and REJECT headlines, and it says which areas require
reading the full file first. No rule text was deleted or reworded.

| Original (stub, auto-loaded) | Full text (read before touching the area) |
|---|---|
| `.claude/rules/project/<name>.md` | `docs/claude-rules-full/project/<name>.md` |

Conventions:

- **The full file is authoritative.** Where a stub and its full file differ,
  the full file wins.
- **Rule-file-first law.** Record a new dated operator quote in the FULL file,
  then update the stub's summary in the same change if a binding rule or a
  REJECT row changed.
- Guard tests that pin phrases in these rules read the FULL files here.
  `crates/common/tests/error_code_rule_file_crossref.rs` scans this directory
  alongside `.claude/rules/` and `docs/error-runbooks/`.
- `ErrorCode::runbook_path()` values that point at a stub path still resolve,
  because the stub exists.

Precedents: `docs/rules-archive/` (retired/historical sections, 2026-07-20) and
`docs/error-runbooks/` (per-error-code runbooks, 2026-07-20).

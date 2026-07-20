# Rules Archive — historical/retired rule content

Historical/retired rule content moved out of the auto-loaded `.claude/rules/`
tree (context-size incident 2026-07-20: the rules tree grew past ~775KB and
was injected into every Claude session's base context, tripping the
autocompact breaker and killing sessions). Nothing here is deleted; live rule
files carry pointers. All operator quotes and audit history survive verbatim
in these files — SEBI/audit posture unchanged, just not auto-loaded.

- Whole-file archives keep their original filename; a ≤5-line stub remains at
  the old `.claude/rules/` path (runbook_path / cross-ref tests need the path
  to exist and live ErrorCode strings to stay mentioned under `.claude/rules/`).
- Section archives (`<name>-archive.md`) hold RETIRED / SUPERSEDED /
  historical-audit sections excised from live rule files; each excision site
  in the live file carries a one-line pointer here.

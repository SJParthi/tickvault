---
name: phase
description: Read the current phase document and summarize what needs to be built
disable-model-invocation: true
allowed-tools: Read, Glob, Grep
---

Read the current project phase and provide a status report.

## Steps

1. Find the current phase doc: `docs/phases/PHASE_*.md`
2. Read the phase document
3. Read `git log --oneline -20` to see recent work
4. Compare what the phase requires vs what's been done

## Output

Provide a concise summary:
- **Phase:** name and number
- **Goal:** one-line summary
- **Done:** completed items (from git log)
- **Remaining:** items still to build
- **Next:** the single most important next step

Keep it under 20 lines. No essays.

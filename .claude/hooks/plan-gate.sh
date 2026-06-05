#!/bin/bash
# plan-gate.sh — THE DESIGN-FIRST WALL.
#
# Operator's hardest requirement (2026-06-05, verbatim intent):
#   "until or unless thoroughly designing/deriving an extensive plan covering
#    everything we should NEVER EVER go ahead with implementation."
#
# This is the mechanical wall that turns that from a wish into a block: if a
# change touches implementation source (crates/<x>/src/**.rs) there MUST be an
# APPROVED active plan that covers, at minimum:
#     Design · Edge Cases · Failure Modes · Test Plan · Rollback · Observability
# Missing any of those → the push/PR is BLOCKED.
#
# HONEST ENVELOPE (no illusion): this gate enforces that the *known* design
# surface is covered before code is written. It cannot invent unknown unknowns,
# and it only fires where it is wired (pre-push-gate.sh + the version-controlled
# scripts/git-hooks/pre-push installed by bootstrap). It is fail-CLOSED on a real
# violation and PASS on non-implementation changes (docs/CI/hooks/config/deps).
#
# Usage:  plan-gate.sh <PROJECT_DIR>
# Exit:   0 = allowed   ·   2 = BLOCK
#
# Escape hatch (auditable, loud): put `PLAN-EXEMPT: <reason>` in the latest
# commit message body for a genuinely trivial change. Every use is echoed.

set -uo pipefail

PROJECT_DIR="${1:-.}"
cd "$PROJECT_DIR" 2>/dev/null || { echo "  plan-gate: cannot cd to $PROJECT_DIR" >&2; exit 2; }

# ── Which files does this push change? (committed-but-unpushed + working tree) ──
BASE="${PLAN_GATE_BASE:-origin/main}"
CHANGED="$(
  { git diff --name-only "${BASE}...HEAD" 2>/dev/null
    git diff --name-only HEAD 2>/dev/null
    git diff --name-only --cached 2>/dev/null
  } | sort -u | sed '/^$/d'
)"

# ── Does it touch implementation logic? ──
IMPL="$(printf '%s\n' "$CHANGED" | grep -E '^crates/[a-z_]+/src/.*\.rs$' || true)"
if [ -z "$IMPL" ]; then
  echo "  plan-gate: PASS — no crates/*/src/*.rs logic changed (docs/CI/hooks/config/deps are exempt)" >&2
  exit 0
fi

# ── Auditable exemption for a trivial logic change ──
LAST_MSG="$(git log -1 --pretty=%B 2>/dev/null || true)"
if printf '%s' "$LAST_MSG" | grep -qE '^PLAN-EXEMPT:[[:space:]]*\S'; then
  REASON="$(printf '%s' "$LAST_MSG" | grep -m1 -E '^PLAN-EXEMPT:' | sed 's/^PLAN-EXEMPT:[[:space:]]*//')"
  echo "  plan-gate: PASS (EXEMPT) — operator-acknowledged trivial change: ${REASON}" >&2
  echo "  plan-gate: NOTE — exemptions are logged; review they are not hiding un-designed work." >&2
  exit 0
fi

# ── Implementation logic changed → require an APPROVED extensive plan ──
PLAN="$(ls .claude/plans/active-plan*.md 2>/dev/null | head -1)"
if [ -z "$PLAN" ]; then
  echo "  plan-gate: BLOCK — implementation source changed but NO active plan exists." >&2
  printf '%s\n' "$IMPL" | sed 's/^/      changed: /' >&2
  echo "  THE DESIGN-FIRST WALL: write .claude/plans/active-plan.md and design it FIRST." >&2
  echo "  (Or, for a genuinely trivial change, add 'PLAN-EXEMPT: <reason>' to the commit body.)" >&2
  exit 2
fi

# Required design sections — each must appear as a markdown heading.
declare -a REQUIRED=("Design" "Edge[ -]?Cases" "Failure[ -]?Modes" "Test[ -]?Plan" "Rollback" "Observability")
declare -a LABELS=("Design" "Edge Cases" "Failure Modes" "Test Plan" "Rollback" "Observability")
MISSING=""
for i in "${!REQUIRED[@]}"; do
  if ! grep -qiE "^#{1,6}[[:space:]].*${REQUIRED[$i]}" "$PLAN"; then
    MISSING="${MISSING}\n      - missing section: ## ${LABELS[$i]}"
  fi
done

# Status must be APPROVED / IN_PROGRESS / VERIFIED (i.e. designed + signed off).
STATUS="$(grep -m1 -iE '^\*\*Status:\*\*' "$PLAN" 2>/dev/null | sed 's/.*\*\*[Ss]tatus:\*\*//' | tr -d '[:space:]' | tr '[:lower:]' '[:upper:]')"
case "$STATUS" in
  APPROVED|IN_PROGRESS|VERIFIED) : ;;
  *) MISSING="${MISSING}\n      - Status is '${STATUS:-<none>}' (must be APPROVED before implementation)";;
esac

if [ -n "$MISSING" ]; then
  echo "  plan-gate: BLOCK — $PLAN does not yet cover everything before implementation:" >&2
  echo -e "$MISSING" >&2
  echo "  Fill every section + set **Status:** APPROVED, then push again (the design-first wall)." >&2
  exit 2
fi

echo "  plan-gate: PASS — design-first wall satisfied ($PLAN, status=$STATUS)" >&2
exit 0

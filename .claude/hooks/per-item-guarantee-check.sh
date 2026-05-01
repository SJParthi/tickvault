#!/usr/bin/env bash
# .claude/hooks/per-item-guarantee-check.sh
#
# Mechanical enforcement of the per-wave / per-item guarantee matrix
# defined in .claude/rules/project/per-wave-guarantee-matrix.md.
#
# Scans every .claude/plans/active-plan*.md file. For each one, verifies:
#   1. Either it carries the canonical 15-row 100% guarantee matrix, OR
#      it cross-references it via "see Item 22 / Item 24 / per-wave-guarantee-matrix.md".
#   2. Either it carries the 7-row resilience demand matrix, OR
#      it cross-references the same.
#   3. The "100% guarantee" phrase, if used, is qualified with the envelope
#      wording from `wave-4-shared-preamble.md` §8.
#
# Exit 0 = PASS. Exit 2 = BLOCK (one or more plans missing matrix).
#
# Per CLAUDE.md "Hooks use stderr for output" rule.

set -uo pipefail  # not -e; we want to enumerate ALL violations, not stop at first

PROJECT_DIR="${CLAUDE_PROJECT_DIR:-$(pwd)}"
PLANS_DIR="$PROJECT_DIR/.claude/plans"
RULE_FILE="$PROJECT_DIR/.claude/rules/project/per-wave-guarantee-matrix.md"

if [[ ! -d "$PLANS_DIR" ]]; then
  echo "[per-item-guarantee-check] $PLANS_DIR missing — skip" >&2
  exit 0
fi

if [[ ! -f "$RULE_FILE" ]]; then
  echo "[per-item-guarantee-check] FAIL: rule file missing: $RULE_FILE" >&2
  exit 2
fi

VIOLATIONS=0
PASSES=0
SKIPS=0

# Sentinel phrases from the canonical rule file. Plans MUST mention at least
# one of these to be considered "carrying the matrix" (either inline or by
# cross-reference). The cross-reference style is preferred for brevity.
SENTINELS=(
  "100% code coverage"          # 15-row matrix sentinel
  "Zero ticks lost"             # 7-row resilience matrix sentinel
  "per-wave-guarantee-matrix.md" # cross-reference to the canonical rule
  "Per-Item Guarantee Matrix"   # Item 22 wording
  "Plan-Wide Proof Matrix"      # Item 24 wording
)

for plan in "$PLANS_DIR"/active-plan*.md; do
  [[ -e "$plan" ]] || continue   # glob match nothing → skip

  name="$(basename "$plan")"

  # SKIP archived / superseded plans (they don't need the matrix going forward).
  if grep -qE '^\*\*Status:\*\*\s+(VERIFIED|ARCHIVED|SUPERSEDED)' "$plan"; then
    echo "[per-item-guarantee-check] SKIP $name (status terminal)" >&2
    SKIPS=$((SKIPS + 1))
    continue
  fi

  found=0
  for sentinel in "${SENTINELS[@]}"; do
    if grep -qF "$sentinel" "$plan"; then
      found=1
      break
    fi
  done

  if [[ $found -eq 1 ]]; then
    echo "[per-item-guarantee-check] PASS $name" >&2
    PASSES=$((PASSES + 1))
  else
    echo "[per-item-guarantee-check] FAIL $name" >&2
    echo "  Plan does not carry the 15-row 100% Guarantee Matrix or the 7-row" >&2
    echo "  Resilience Demand Matrix from per-wave-guarantee-matrix.md, and" >&2
    echo "  does not cross-reference Item 22 / Item 24 / the canonical rule." >&2
    echo "  Add ONE of:" >&2
    echo "    - The full 15-row + 7-row matrices inline (see Item 22 / Item 24" >&2
    echo "      in active-plan-wave-5-indices-only.md for the canonical form)" >&2
    echo "    - A short subsection cross-referencing 'See per-wave-guarantee-matrix.md'" >&2
    echo "      and confirming all 15 + 7 rows apply to every item in this plan" >&2
    VIOLATIONS=$((VIOLATIONS + 1))
  fi
done

# Honest 100% claim wording check — if any plan uses the phrase
# "100% guarantee" it MUST also use the envelope qualifier.
for plan in "$PLANS_DIR"/active-plan*.md; do
  [[ -e "$plan" ]] || continue
  name="$(basename "$plan")"
  if grep -qE 'Status:.*(VERIFIED|ARCHIVED|SUPERSEDED)' "$plan"; then continue; fi

  if grep -qE '100\s*(%|percent|percentage)\s+guarantee' "$plan"; then
    if ! grep -qE 'inside the tested envelope|ratcheted regression coverage|honest charter|honest non-claim' "$plan"; then
      echo "[per-item-guarantee-check] FAIL $name" >&2
      echo "  Plan uses '100% guarantee' phrase WITHOUT envelope qualifier." >&2
      echo "  Per wave-4-shared-preamble.md §8, the phrase MUST be qualified" >&2
      echo "  with 'inside the tested envelope, with ratcheted regression" >&2
      echo "  coverage' OR an explicit 'honest non-claim' subsection." >&2
      VIOLATIONS=$((VIOLATIONS + 1))
    fi
  fi
done

echo "" >&2
echo "[per-item-guarantee-check] summary: $PASSES PASS, $VIOLATIONS FAIL, $SKIPS SKIP" >&2

if [[ $VIOLATIONS -gt 0 ]]; then
  exit 2
fi
exit 0

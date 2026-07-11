#!/bin/bash
# plan-gate.sh — THE DESIGN-FIRST WALL.
#
# Operator's hardest requirement (2026-06-05, verbatim intent):
#   "until or unless thoroughly designing/deriving an extensive plan covering
#    everything we should NEVER EVER go ahead with implementation."
#
# This is the mechanical wall that turns that from a wish into a block: if a
# change touches implementation source (crates/<x>/src/**.rs) there MUST be an
# APPROVED active plan that:
#   - has all 6 design sections, each with a NON-EMPTY body:
#       Design · Edge Cases · Failure Modes · Test Plan · Rollback · Observability
#   - is marked Status APPROVED / IN_PROGRESS / VERIFIED
#   - references at least one crate (or file) actually being changed
# Missing any of those → the push/PR is BLOCKED.
#
# HONEST ENVELOPE (no illusion): this gate enforces that the *known* design
# surface is covered before code is written. It cannot invent unknown unknowns,
# and it only fires where it is wired (pre-push-gate.sh + the version-controlled
# scripts/git-hooks/pre-push installed by bootstrap). It is fail-CLOSED on a real
# violation and PASS on non-implementation changes (docs/CI/hooks/config/deps).
#
# Adversarial-review hardening (2026-06-05, security-reviewer + hostile agent):
#   C1  untracked brand-new *.rs files ARE counted (git ls-files --others)
#   H2  crate-name regex accepts digits/uppercase/hyphens
#   H3  ALL active plans are scanned; the satisfying one must reference the change
#   M4  each required section must have a non-empty body (not just a heading)
#   M5  PLAN-EXEMPT covers at most ONE implementation file (no whole-feature bypass)
#   L6  base ref resolves origin/main → main, NUL-safe path handling throughout
#
# Stale-plan-pile hardening (2026-07-10 audit — the 107-stale-plan incident):
#   V7  the gate REFUSES to evaluate when more than PLAN_GATE_MAX_ACTIVE
#       (default 5) active-plan*.md files exist. With ~107 merged-but-never-
#       archived APPROVED/VERIFIED plans collectively referencing every crate,
#       ANY impl change matched SOME stale plan — H3's crate-reference check
#       was defeated and the wall was VACUOUS. The cap makes accumulation
#       itself trip the gate: archive merged plans to .claude/plans/archive/
#       (per plan-enforcement.md) and the wall regains discriminating power.
#       HONEST LIMIT: this stops ACCIDENTAL vacuous passes from stale piles;
#       it does NOT stop deliberate forgery (fabricating/backdating a plan
#       that names your crate) — plan CONTENT quality remains a review job.
#
# Usage:  plan-gate.sh <PROJECT_DIR>
# Exit:   0 = allowed   ·   2 = BLOCK
#
# Escape hatch (auditable, loud, capped): put `PLAN-EXEMPT: <reason>` in the
# latest commit body for a genuinely trivial change touching ≤1 impl file.

set -uo pipefail

PROJECT_DIR="${1:-.}"
cd "$PROJECT_DIR" 2>/dev/null || { echo "  plan-gate: cannot cd to $PROJECT_DIR" >&2; exit 2; }

# ── Resolve the base ref (L6): origin/main → main → none ──────────────────────
BASE=""
if [ -n "${PLAN_GATE_BASE:-}" ]; then
  BASE="$PLAN_GATE_BASE"
elif git rev-parse --verify -q origin/main >/dev/null 2>&1; then
  BASE="origin/main"
elif git rev-parse --verify -q main >/dev/null 2>&1; then
  BASE="main"
fi

# ── Which files does this push change? ────────────────────────────────────────
# Committed-vs-base + working tree + staged + UNTRACKED (C1). NUL-safe (L6/H2).
declare -A _seen
IMPL=()
while IFS= read -r -d '' f; do
  [ -z "$f" ] && continue
  [ -n "${_seen[$f]:-}" ] && continue
  _seen[$f]=1
  # H2: crate dir may contain digits / uppercase / hyphens / underscores.
  if printf '%s' "$f" | grep -qE '^crates/[A-Za-z0-9_-]+/src/.*\.rs$'; then
    IMPL+=("$f")
  fi
done < <(
  { [ -n "$BASE" ] && git diff -z --name-only "${BASE}...HEAD" 2>/dev/null
    git diff -z --name-only HEAD 2>/dev/null
    git diff -z --name-only --cached 2>/dev/null
    git ls-files -z --others --exclude-standard 2>/dev/null
  }
)

# ── Does it touch implementation logic? ──
if [ "${#IMPL[@]}" -eq 0 ]; then
  echo "  plan-gate: PASS — no crates/*/src/*.rs logic changed (docs/CI/hooks/config/deps are exempt)" >&2
  exit 0
fi

# ── Auditable, CAPPED exemption for a trivial logic change (M5) ──
LAST_MSG="$(git log -1 --pretty=%B 2>/dev/null || true)"
if printf '%s' "$LAST_MSG" | grep -qE '^PLAN-EXEMPT:[[:space:]]*\S'; then
  REASON="$(printf '%s' "$LAST_MSG" | grep -m1 -E '^PLAN-EXEMPT:' | sed 's/^PLAN-EXEMPT:[[:space:]]*//')"
  if [ "${#IMPL[@]}" -le 1 ]; then
    echo "  plan-gate: PASS (EXEMPT) — operator-acknowledged trivial change: ${REASON}" >&2
    echo "  plan-gate: NOTE — exemptions are logged; review they are not hiding un-designed work." >&2
    exit 0
  fi
  echo "  plan-gate: BLOCK — PLAN-EXEMPT covers ≤1 implementation file, but ${#IMPL[@]} changed:" >&2
  printf '%s\n' "${IMPL[@]}" | sed 's/^/      changed: /' >&2
  echo "  A multi-file implementation change needs a real plan, not an exemption." >&2
  exit 2
fi

# ── Touched crate names (for the plan-references-the-change check, H3) ──
declare -A CRATES_TOUCHED
for f in "${IMPL[@]}"; do
  c="$(printf '%s' "$f" | sed -nE 's|^crates/([A-Za-z0-9_-]+)/src/.*|\1|p')"
  [ -n "$c" ] && CRATES_TOUCHED[$c]=1
done

# ── Helpers ───────────────────────────────────────────────────────────────────

# section_has_body <file> <lowercase-heading-regex>
# True (0) iff a heading matching the regex exists AND has ≥1 non-blank,
# non-heading line before the next heading (M4 — empty headings don't count).
section_has_body() {
  awk -v kw="$2" '
    function isheading(s){ return (s ~ /^#+[ \t]/) }
    BEGIN{ inSec=0; found=0 }
    {
      if (isheading($0)) {
        if (inSec==1) { exit }
        if (tolower($0) ~ kw) { inSec=1 }
        next
      }
      if (inSec==1) {
        t=$0; gsub(/[ \t\r]/,"",t)
        if (t!="") { found=1; exit }
      }
    }
    END{ exit (found?0:1) }
  ' "$1"
}

# plan_references_change <file> — true if the plan mentions a touched crate or impl path (H3).
plan_references_change() {
  local file="$1" c f
  for c in "${!CRATES_TOUCHED[@]}"; do
    grep -qiw "$c" "$file" 2>/dev/null && return 0
  done
  for f in "${IMPL[@]}"; do
    grep -qF "$f" "$file" 2>/dev/null && return 0
  done
  return 1
}

# Required design sections — lowercase ERE matched against the (lowercased) heading.
REQUIRED=("design" "edge[ -]?cases" "failure[ -]?modes" "test[ -]?plan" "rollback" "observability")
LABELS=("Design" "Edge Cases" "Failure Modes" "Test Plan" "Rollback" "Observability")

# check_plan <file> — echoes the reasons it is NOT satisfying (empty = satisfied).
check_plan() {
  local plan="$1" miss="" i status
  for i in "${!REQUIRED[@]}"; do
    if ! section_has_body "$plan" "${REQUIRED[$i]}"; then
      miss="${miss}\n        - missing/empty section: ## ${LABELS[$i]}"
    fi
  done
  status="$(grep -m1 -iE '^\*\*Status:\*\*' "$plan" 2>/dev/null | sed 's/.*\*\*[Ss]tatus:\*\*//' | tr -d '[:space:]' | tr '[:lower:]' '[:upper:]')"
  case "$status" in
    APPROVED|IN_PROGRESS|VERIFIED) : ;;
    *) miss="${miss}\n        - Status is '${status:-<none>}' (must be APPROVED before implementation)";;
  esac
  if ! plan_references_change "$plan"; then
    miss="${miss}\n        - plan does not reference any changed crate (${!CRATES_TOUCHED[*]}) or file"
  fi
  printf '%b' "$miss"
}

# ── Implementation logic changed → require a SATISFYING approved plan ──
shopt -s nullglob
PLANS=(.claude/plans/active-plan*.md)
shopt -u nullglob
if [ "${#PLANS[@]}" -eq 0 ]; then
  echo "  plan-gate: BLOCK — implementation source changed but NO active plan exists." >&2
  printf '%s\n' "${IMPL[@]}" | sed 's/^/      changed: /' >&2
  echo "  THE DESIGN-FIRST WALL: write .claude/plans/active-plan.md and design it FIRST." >&2
  echo "  (Or, for a genuinely trivial ≤1-file change, add 'PLAN-EXEMPT: <reason>' to the commit body.)" >&2
  exit 2
fi

# ── V7 (2026-07-10): a PILE of active plans makes the wall vacuous — refuse ──
# With many merged-but-unarchived APPROVED/VERIFIED plans, some stale plan
# references every crate and H3 is defeated. Fail LOUD until the pile is
# archived. CI never sets PLAN_GATE_MAX_ACTIVE, so the server-side wall
# always enforces the default cap; the env knob exists for the selftest.
MAX_ACTIVE="${PLAN_GATE_MAX_ACTIVE:-5}"
# Fail-closed on a garbage override: a non-integer knob must not disable V7.
if ! printf '%s' "$MAX_ACTIVE" | grep -qE '^[0-9]+$'; then MAX_ACTIVE=5; fi
if [ "${#PLANS[@]}" -gt "$MAX_ACTIVE" ]; then
  echo "  plan-gate: BLOCK — ${#PLANS[@]} active-plan files exist (cap: ${MAX_ACTIVE})." >&2
  echo "  A pile of stale active plans makes the design-first wall VACUOUS: any impl" >&2
  echo "  change matches SOME stale plan (the 2026-07-10 107-stale-plan incident)." >&2
  echo "  Fix: archive every plan whose work is merged to" >&2
  echo "      .claude/plans/archive/YYYY-MM-DD-<slug>.md   (per plan-enforcement.md)" >&2
  echo "  keeping ONLY genuinely-active plans, then push again." >&2
  exit 2
fi

# Scan ALL active plans (H3) — at least one must fully satisfy the change.
ALL_REASONS=""
for PLAN in "${PLANS[@]}"; do
  REASONS="$(check_plan "$PLAN")"
  if [ -z "$REASONS" ]; then
    echo "  plan-gate: PASS — design-first wall satisfied ($PLAN)" >&2
    exit 0
  fi
  ALL_REASONS="${ALL_REASONS}\n    ${PLAN}:${REASONS}"
done

echo "  plan-gate: BLOCK — no active plan fully covers this implementation change:" >&2
printf '%s\n' "${IMPL[@]}" | sed 's/^/      changed: /' >&2
printf '%b\n' "$ALL_REASONS" >&2
echo "  Fill every section (with content), set **Status:** APPROVED, and reference the" >&2
echo "  changed crate in the plan — then push again (the design-first wall)." >&2
exit 2

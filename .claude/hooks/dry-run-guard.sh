#!/bin/bash
# dry-run-guard.sh — SAFETY: Blocks any commit that sets dry_run to false.
# This is a mechanical enforcement to ensure NO REAL ORDERS are placed
# until the entire product is finalized and explicitly approved.
# Called by pre-commit-gate.sh. Exit 2 = block commit.

set -euo pipefail

PROJECT_DIR="${1:-.}"
STAGED_FILES="${2:-}"

# If no staged files passed, detect them
if [ -z "$STAGED_FILES" ]; then
  STAGED_FILES=$(cd "$PROJECT_DIR" && git diff --cached --name-only --diff-filter=ACMR 2>/dev/null || true)
fi

if [ -z "$STAGED_FILES" ]; then
  exit 0
fi

VIOLATIONS=0
REPORT=""

while IFS= read -r file; do
  [ -z "$file" ] && continue
  filepath="$PROJECT_DIR/$file"
  [ -f "$filepath" ] || continue

  # Check for dry_run = false in TOML config files
  if echo "$file" | grep -qE '\.(toml|yaml|yml|json)$'; then
    if grep -nE '(dry_run\s*=\s*false|"dry_run"\s*:\s*false|dry_run:\s*false)' "$filepath" 2>/dev/null; then
      REPORT+="  BLOCKED: $file sets dry_run = false (NO REAL ORDERS ALLOWED)\n"
      VIOLATIONS=$((VIOLATIONS + 1))
    fi
  fi

  # Check for dry_run being set to false in Rust code (not in test blocks)
  if echo "$file" | grep -qE '\.rs$'; then
    # Only check production code (not test modules)
    if grep -nE 'dry_run:\s*false|dry_run\s*=\s*false' "$filepath" 2>/dev/null | grep -v '#\[cfg(test)\]' | grep -v 'mod tests' | grep -v '/// ' | grep -v '///' | grep -v '// ' | head -5; then
      # Allow: default_dry_run() which returns true, and doc comments
      # Block: actual struct initialization with dry_run: false
      if grep -E 'dry_run:\s*false' "$filepath" 2>/dev/null | grep -v 'fn default_dry_run' | grep -v '#\[cfg(test)\]' | grep -v '/// ' | grep -v '//' | grep -v 'assert' > /dev/null 2>&1; then
        REPORT+="  BLOCKED: $file contains dry_run: false in production code\n"
        VIOLATIONS=$((VIOLATIONS + 1))
      fi
    fi
  fi
done <<< "$STAGED_FILES"

if [ "$VIOLATIONS" -gt 0 ]; then
  echo "=== DRY-RUN SAFETY GUARD ==="
  echo "BLOCKED: $VIOLATIONS file(s) attempt to disable dry-run mode."
  echo "NO REAL ORDERS are permitted until the entire product is finalized."
  echo ""
  echo -e "$REPORT"
  echo "To fix: ensure dry_run = true in all config files and production code."
  exit 2
fi

exit 0

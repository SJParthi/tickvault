#!/usr/bin/env bash
# validate-deploy-ssm-quoting.sh
#
# Guard against the 2026-06-09 deploy break (CI run 27210316913):
# the SSM `--parameters 'commands=[ ... ]'` block in the deploy workflows is a
# SINGLE-QUOTED string on the GitHub Actions runner. A literal apostrophe (')
# inside that block CLOSES the outer single quote early; any following `(` then
# parses as an unquoted subshell token →
#     /home/runner/work/_temp/xxx.sh: line N: syntax error near unexpected token `('
# and the whole deploy fails before the new binary is ever swapped in.
#
# This script fails (exit 2) if any line BETWEEN the opening `'commands=[` and the
# closing `]'` of a deploy workflow contains a literal apostrophe. The opening and
# closing lines themselves are the only legal apostrophes.
#
# Box-side quoting MUST use escaped double quotes (\") — never raw single quotes.
set -euo pipefail

ROOT="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
FILES=(
  "$ROOT/.github/workflows/deploy-aws.yml"
  "$ROOT/.github/workflows/deploy-aws-after-close.yml"
  "$ROOT/.github/workflows/downsize-instance.yml"
)

rc=0
for f in "${FILES[@]}"; do
  [ -f "$f" ] || continue
  # awk: track whether we are inside a MULTI-LINE `commands=[ ... ]` block.
  # A single-line `commands=["..."]'` opener (close `]'` on the same physical
  # line) is parsed atomically by the runner and is NOT entered as a block.
  # Inside a multi-line block, flag any interior line carrying an apostrophe.
  bad=$(awk '
    # multi-line opener: has commands=[ but NO closing ]'\'' on the same line
    /--parameters '\''commands=\[/ && $0 !~ /\]'\''/ { inblock=1; next }
    # closer of a multi-line block
    inblock && /^[[:space:]]*\]'\''/ { inblock=0; next }
    # interior line of a multi-line block with a quote-breaking apostrophe
    inblock && /'\''/ { print FILENAME ":" NR ": " $0 }
  ' "$f" || true)
  if [ -n "$bad" ]; then
    echo "FAIL: apostrophe inside SSM commands=[...] block (breaks the outer single quote):" >&2
    echo "$bad" >&2
    echo "  Fix: use escaped double quotes \\\" for box-side strings; never a raw single quote." >&2
    rc=2
  fi
done

if [ "$rc" -eq 0 ]; then
  echo "OK: no quote-breaking apostrophe inside any deploy SSM commands=[...] block."
fi
exit "$rc"

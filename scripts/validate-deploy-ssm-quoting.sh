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

  # 2026-08-20 INCIDENT CHECK — over-escaped box-side quotes.
  #
  # The deploy of f697016 FAILED on the box with:
  #     awk: cmd. line:1: "/^MemTotal:/
  #     awk: cmd. line:1:      ^ unterminated string
  # and its failure path then STOPPED the app mid-session. The cause was not
  # an apostrophe (the check above) but the ESCAPING LEVEL: the questdb-mem
  # block added 2026-08-15 wrote \\\" (three backslashes + quote) where every
  # working line in the same array writes \" (one backslash + quote). The
  # extra pair survives into the JSON as a LITERAL backslash-quote, so the
  # box-side program receives a stray " and dies.
  #
  # This is the same family as the 2026-07-02 rc=127 apostrophe bug the check
  # above exists for — a quoting level that is wrong only on the box, so it is
  # invisible until a real deploy runs. Every deploy silently failed and
  # auto-stopped the app, which is why prod sat on a launch-time binary.
  bad_esc=$(awk '
    /--parameters '\''commands=\[/ && $0 !~ /\]'\''/ { inblock=1; next }
    inblock && /^[[:space:]]*\]'\''/ { inblock=0; next }
    inblock && /\\\\\\"/ { print FILENAME ":" NR ": " $0 }
  ' "$f" || true)
  if [ -n "$bad_esc" ]; then
    echo "FAIL: over-escaped quote (\\\\\\\" ) inside SSM commands=[...] block:" >&2
    echo "$bad_esc" >&2
    echo "  The box receives a literal backslash-quote and the command dies." >&2
    echo "  Fix: use \\\" (one backslash + quote), matching every working line." >&2
    rc=2
  fi
done

if [ "$rc" -eq 0 ]; then
  echo "OK: no quote-breaking apostrophe inside any deploy SSM commands=[...] block."
fi
exit "$rc"

#!/bin/bash
# secret-scanner.sh — Detects hardcoded secrets in staged files
# Called by pre-commit-gate.sh. Exit 2 = block commit.
# Layer 1 enforcement: no secrets on disk, ever.

set -euo pipefail

PROJECT_DIR="${1:-.}"
STAGED_FILES="${2:-}"

if [ -z "$STAGED_FILES" ]; then
  STAGED_FILES=$(cd "$PROJECT_DIR" && git diff --cached --name-only --diff-filter=ACMR 2>/dev/null || true)
fi

if [ -z "$STAGED_FILES" ]; then
  exit 0
fi

VIOLATIONS=0
REPORT=""

scan_secret() {
  local pattern="$1"
  local description="$2"

  while IFS= read -r file; do
    [ -z "$file" ] && continue
    local full_path="$PROJECT_DIR/$file"
    [ ! -f "$full_path" ] && continue

    # Skip binary files and lock files
    if echo "$file" | grep -qE '\.(lock|png|jpg|gif|ico|woff|woff2|ttf|eot)$'; then
      continue
    fi

    # Skip CI workflows that contain detection patterns (not real secrets)
    if echo "$file" | grep -qE '(secret-scan|security-audit)\.yml$'; then
      continue
    fi

    # Skip this scanner itself (contains secret patterns as detection rules)
    if echo "$file" | grep -qE 'secret-scanner\.sh$'; then
      continue
    fi

    # Only scan relevant file types for secrets (including .sh and extensionless like Dockerfile)
    local basename_file
    basename_file=$(basename "$file")
    if ! echo "$file" | grep -qE '\.(rs|toml|json|yml|yaml|cfg|conf|ini|properties|sh|env|txt)$' && \
       ! echo "$basename_file" | grep -qiE '^(Dockerfile|Makefile|Justfile|Procfile)$'; then
      continue
    fi

    # Skip test files entirely (test fixtures may have fake secrets)
    if echo "$file" | grep -qE '(_test\.rs|/tests/|/test_|_tests\.rs|/benches/|/fixtures/)'; then
      continue
    fi

    local matches
    matches=$(grep -n -i "$pattern" "$full_path" 2>/dev/null \
      | grep -v '// test' \
      | grep -v '/// ' \
      | grep -v '#\[doc' \
      | grep -v '#\[cfg(test)\]' \
      | grep -v 'ssm_parameter' \
      | grep -v 'Parameter' \
      | grep -v 'secret_manager' \
      | grep -v 'SecretManager' \
      | grep -v 'Secret<' \
      | grep -v 'get_secret' \
      | grep -v 'fetch_secret' \
      | grep -v 'fetch_ssm_secret' \
      | grep -v 'rotate_secret' \
      | grep -v '_SECRET: &str' \
      | grep -v 'REDACTED' \
      | grep -v 'placeholder' \
      | grep -v 'example' \
      | grep -v 'dummy' \
      | grep -v 'mock' \
      | grep -v 'test_' \
      | grep -v 'fake' \
      | grep -v 'stub' \
      | grep -v 'fixture' \
      | grep -v 'test-' \
      || true)

    if [ -n "$matches" ]; then
      REPORT="${REPORT}\n  [SECRET] $description in $file:"
      while IFS= read -r match; do
        # Truncate long lines to avoid leaking the secret
        local truncated
        truncated=$(echo "$match" | cut -c1-80)
        REPORT="${REPORT}\n    ${truncated}..."
        VIOLATIONS=$((VIOLATIONS + 1))
      done <<< "$matches"
    fi
  done <<< "$STAGED_FILES"
}

echo "=== Secret Scanner ===" >&2

# API keys and tokens
scan_secret 'api[_-]key.*=.*"[A-Za-z0-9]' 'Hardcoded API key'
scan_secret 'api[_-]secret.*=.*"[A-Za-z0-9]' 'Hardcoded API secret'
scan_secret 'access[_-]token.*=.*"[A-Za-z0-9]' 'Hardcoded access token'
scan_secret 'auth[_-]token.*=.*"[A-Za-z0-9]' 'Hardcoded auth token'

# AWS credentials
scan_secret 'AKIA[0-9A-Z]{16}' 'AWS Access Key ID'
scan_secret 'aws_secret_access_key.*=.*"' 'AWS Secret Access Key'

# Dhan-specific
scan_secret 'client[_-]id.*=.*"[0-9]{10,}' 'Hardcoded Dhan client ID'
scan_secret 'dhan.*token.*=.*"eyJ' 'Hardcoded Dhan JWT token'

# Generic secrets
scan_secret 'password.*=.*"[^"]' 'Hardcoded password'
scan_secret 'passwd.*=.*"[^"]' 'Hardcoded password'
scan_secret 'private[_-]key.*=.*"' 'Hardcoded private key'
scan_secret 'BEGIN.*PRIVATE KEY' 'Embedded private key'
scan_secret 'bearer.*[A-Za-z0-9_-]{20,}' 'Hardcoded bearer token'

# Telegram bot tokens
scan_secret 'bot[0-9]{8,}:' 'Hardcoded Telegram bot token'

# Connection strings with credentials
scan_secret 'postgresql://[^:]*:[^@]*@' 'Database connection string with password'
scan_secret 'redis://:[^@]*@' 'Redis connection string with password'

if [ "$VIOLATIONS" -gt 0 ]; then
  echo "" >&2
  echo "BLOCKED: $VIOLATIONS potential secret(s) found in staged files:" >&2
  echo -e "$REPORT" >&2
  echo "" >&2
  echo "Secrets must come from AWS SSM Parameter Store, never hardcoded." >&2
  echo "Use Secret<String> wrapper. See docs/standards/secret-rotation.md" >&2
  exit 2
fi

echo "  Secret scan: CLEAN" >&2
exit 0

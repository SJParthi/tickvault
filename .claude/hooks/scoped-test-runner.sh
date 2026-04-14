#!/usr/bin/env bash
# scoped-test-runner.sh — run cargo tests only for crates touched in diff
#
# Rule: .claude/rules/project/testing-scope.md
#
# Algorithm:
#   1. Compute changed files from git (staged + unstaged + untracked tracked).
#   2. Extract crate names from paths matching crates/<name>/...
#   3. Escalate to workspace if:
#        - FULL_QA=1 in env
#        - crates/common/ was touched (everyone depends on common)
#        - More than 3 crates touched
#        - Cargo.toml workspace deps changed
#        - .claude/rules/ files changed
#   4. Otherwise run: cargo test -p tickvault-<crate> for each touched.
#
# Exit codes:
#   0 — all scoped tests passed (or nothing to test)
#   1 — cargo test failed
#   2 — git not available or not a git repo

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [[ -z "${REPO_ROOT}" ]]; then
    echo "scoped-test-runner: not in a git repo" >&2
    exit 2
fi
cd "${REPO_ROOT}"

# -----------------------------------------------------------------------------
# Step 1: Compute changed files
# -----------------------------------------------------------------------------

CHANGED_FILES="$(
    {
        git diff --name-only HEAD 2>/dev/null || true
        git diff --cached --name-only 2>/dev/null || true
        git ls-files --others --exclude-standard 2>/dev/null || true
    } | sort -u
)"

if [[ -z "${CHANGED_FILES}" ]]; then
    echo "SCOPE: (nothing changed)"
    echo "SKIP : all — no tests to run"
    exit 0
fi

# -----------------------------------------------------------------------------
# Step 2: Extract touched crate names
# -----------------------------------------------------------------------------

TOUCHED_CRATES="$(
    printf '%s\n' "${CHANGED_FILES}" \
        | grep -E '^crates/[^/]+/' \
        | awk -F'/' '{print $2}' \
        | sort -u
)"

# -----------------------------------------------------------------------------
# Step 3: Escalation checks
# -----------------------------------------------------------------------------

ESCALATE=0
ESCALATE_REASON=""

if [[ "${FULL_QA:-0}" == "1" ]]; then
    ESCALATE=1
    ESCALATE_REASON="FULL_QA=1 in env"
fi

if [[ ${ESCALATE} -eq 0 ]] && printf '%s\n' "${TOUCHED_CRATES}" | grep -qx "common"; then
    ESCALATE=1
    ESCALATE_REASON="crates/common/ touched — all downstream crates depend on it"
fi

if [[ ${ESCALATE} -eq 0 ]]; then
    CRATE_COUNT=$(printf '%s\n' "${TOUCHED_CRATES}" | grep -c . || true)
    if [[ "${CRATE_COUNT}" -gt 3 ]]; then
        ESCALATE=1
        ESCALATE_REASON="${CRATE_COUNT} crates touched (>3) — forcing workspace"
    fi
fi

if [[ ${ESCALATE} -eq 0 ]] && printf '%s\n' "${CHANGED_FILES}" | grep -qx "Cargo.toml"; then
    ESCALATE=1
    ESCALATE_REASON="workspace Cargo.toml changed — dep version shift possible"
fi

if [[ ${ESCALATE} -eq 0 ]] && printf '%s\n' "${CHANGED_FILES}" | grep -q '^\.claude/rules/'; then
    ESCALATE=1
    ESCALATE_REASON=".claude/rules/ changed — enforcement rules shifted"
fi

# -----------------------------------------------------------------------------
# Step 4: Print scope summary
# -----------------------------------------------------------------------------

ALL_CRATES="$(find crates -maxdepth 2 -name Cargo.toml -exec dirname {} \; 2>/dev/null \
    | awk -F'/' '{print $2}' | sort -u)"

if [[ ${ESCALATE} -eq 1 ]]; then
    SKIPPED="(none — full workspace)"
    echo "SCOPE: WORKSPACE (full 22-test equivalent)"
    echo "SKIP : ${SKIPPED}"
    echo "REASON: ${ESCALATE_REASON}"
elif [[ -z "${TOUCHED_CRATES}" ]]; then
    echo "SCOPE: (docs/config only — no crate tests to run)"
    echo "SKIP : ${ALL_CRATES}"
    exit 0
else
    SCOPE_LINE="$(printf '%s\n' "${TOUCHED_CRATES}" | tr '\n' ',' | sed 's/,$//')"
    SKIPPED="$(comm -23 <(printf '%s\n' "${ALL_CRATES}") <(printf '%s\n' "${TOUCHED_CRATES}") | tr '\n' ',' | sed 's/,$//')"
    echo "SCOPE: ${SCOPE_LINE}"
    echo "SKIP : ${SKIPPED}"
    echo "REASON: scoped by changed-file analysis (default)"
fi

# -----------------------------------------------------------------------------
# Step 5: Execute
# -----------------------------------------------------------------------------

if [[ ${ESCALATE} -eq 1 ]]; then
    echo "RUN  : cargo test --workspace --lib --tests"
    cargo test --workspace --lib --tests
    exit $?
fi

STATUS=0
for crate in ${TOUCHED_CRATES}; do
    pkg="tickvault-${crate}"
    echo "RUN  : cargo test -p ${pkg} --lib --tests"
    if ! cargo test -p "${pkg}" --lib --tests; then
        STATUS=1
    fi
done

exit ${STATUS}

#!/usr/bin/env bash
# Phase 6 of active-plan.md — cron/shell-driven triage driver.
#
# Complements the Claude `/loop` prompt (`.claude/triage/claude-loop-
# prompt.md`) so the automation still works when no Claude session is
# open. Invoked every 60s by a userland systemd-user timer OR as a
# Claude SessionStart / Stop hook.
#
# Behavior (intentionally simpler than the Claude loop — no auto_fix
# execution, no GitHub Issue opening; those require Claude's tools):
#   1. Read `data/logs/errors.summary.md`. If missing OR "all green",
#      exit 0.
#   2. Read `.claude/triage/error-rules.yaml`. For any signature
#      matching action=silence, append to triage-seen.jsonl.
#   3. For any signature whose rule is action=escalate OR unmatched,
#      write a line to `data/logs/auto-fix.log` and optionally post
#      to Telegram via the notify-telegram.sh helper.
#   4. auto_fix / auto_restart rules are ONLY acted on when invoked
#      with `--execute-autofix`; by default they just log
#      `would_auto_fix`. This keeps the hook safe for cron.
#
# Exit codes:
#   0 — ran cleanly (may have logged / escalated)
#   1 — configuration missing (rules file / summary dir)
#   2 — internal error (malformed YAML, etc.)
set -euo pipefail

EXECUTE_AUTOFIX=0
if [[ "${1:-}" == "--execute-autofix" ]]; then
    EXECUTE_AUTOFIX=1
fi

SUMMARY_FILE="data/logs/errors.summary.md"
RULES_FILE=".claude/triage/error-rules.yaml"
STATE_FILE=".claude/state/triage-seen.jsonl"
AUTO_FIX_LOG="data/logs/auto-fix.log"

mkdir -p "$(dirname "${STATE_FILE}")" "$(dirname "${AUTO_FIX_LOG}")"

TS=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

log() {
    echo "${TS} error-triage $*" | tee -a "${AUTO_FIX_LOG}"
}

if [[ ! -f "${RULES_FILE}" ]]; then
    log "error=rules_file_missing path=${RULES_FILE}"
    exit 1
fi

if [[ ! -f "${SUMMARY_FILE}" ]]; then
    log "noop=no_summary path=${SUMMARY_FILE}"
    exit 0
fi

if grep -q "Zero ERROR-level events" "${SUMMARY_FILE}"; then
    log "status=all_green"
    exit 0
fi

# Extract code strings from the top-signatures table. The markdown
# format renders them in the 3rd column as `| codestr | severity |...`.
# We only need the code strings for rule matching.
CODES=$(grep -oE '\| (I-P[0-9]-[0-9]+|OMS-GAP-[0-9]+|WS-GAP-[0-9]+|GAP-NET-[0-9]+|GAP-SEC-[0-9]+|AUTH-GAP-[0-9]+|RISK-GAP-[0-9]+|STORAGE-GAP-[0-9]+|DH-[0-9]+|DATA-[0-9]+) \|' \
    "${SUMMARY_FILE}" | awk '{print $2}' | sort -u)

if [[ -z "${CODES}" ]]; then
    log "status=no_codes_extracted"
    exit 0
fi

log "status=processing codes_seen=$(echo "${CODES}" | tr '\n' ',' | sed 's/,$//')"

for CODE in ${CODES}; do
    # Crude YAML action lookup: find the block for this code and grep
    # the `action:` line. We accept both quoted and unquoted codes.
    ACTION=$(awk -v code="${CODE}" '
        BEGIN { found=0; action="" }
        /^[[:space:]]*- name:/ { found=0; block_action="" }
        /code: / {
            line=$0
            gsub(/.*code:[[:space:]]*/, "", line)
            gsub(/[[:space:]].*/, "", line)
            if (line == code) { found=1 }
        }
        /action: / && found==1 && block_action=="" {
            block_action=$0
            gsub(/.*action:[[:space:]]*/, "", block_action)
            gsub(/[[:space:]].*/, "", block_action)
            action=block_action
            exit
        }
        END { print action }
    ' "${RULES_FILE}")

    if [[ -z "${ACTION}" ]]; then
        ACTION="escalate"
        log "code=${CODE} action=escalate reason=no_matching_rule"
        echo "{\"signature\":\"-\",\"code\":\"${CODE}\",\"action\":\"escalate_unmatched\",\"ts\":$(date +%s)}" \
            >> "${STATE_FILE}"
        continue
    fi

    case "${ACTION}" in
        silence)
            log "code=${CODE} action=silence"
            echo "{\"signature\":\"-\",\"code\":\"${CODE}\",\"action\":\"silence\",\"ts\":$(date +%s)}" \
                >> "${STATE_FILE}"
            ;;
        escalate)
            log "code=${CODE} action=escalate"
            echo "{\"signature\":\"-\",\"code\":\"${CODE}\",\"action\":\"escalate\",\"ts\":$(date +%s)}" \
                >> "${STATE_FILE}"
            ;;
        auto_fix|auto_restart)
            if [[ "${EXECUTE_AUTOFIX}" == "1" ]]; then
                log "code=${CODE} action=${ACTION} execute=yes note=hook_delegates_to_claude_loop"
            else
                log "code=${CODE} action=${ACTION} execute=no reason=use_execute_autofix_flag"
            fi
            echo "{\"signature\":\"-\",\"code\":\"${CODE}\",\"action\":\"${ACTION}_deferred\",\"ts\":$(date +%s)}" \
                >> "${STATE_FILE}"
            ;;
        *)
            log "code=${CODE} action=unknown raw=${ACTION}"
            ;;
    esac
done

log "status=done"
exit 0

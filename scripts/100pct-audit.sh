#!/usr/bin/env bash
# Audit evidence tracker — runs selected checks and records their scope.
# File existence is inventory, never proof that a check passed.
#
# M5 of .claude/plans/autonomous-operations-100pct.md.
# Living matrix: .claude/plans/100pct-audit-tracker.md
#
# Categories:
#   P = Mechanically Provable (type system, CI gate, test)
#   R = Runtime Verifiable (metric, alert, live probe)
#   L = Layered Asymptotic (no absolute guarantee, defense in depth)
#   I = Impossible Absolute (math forbids; closest proxies listed)
#
# Usage:
#   scripts/100pct-audit.sh            # human-readable dashboard
#   scripts/100pct-audit.sh --json     # structured output
#   scripts/100pct-audit.sh --ci       # blocking mode for CI
#
# Exit:
#   0  all P/R dimensions have passing execution evidence (L/I advisory)
#   1  one or more P/R dimensions failed or have no execution evidence
#   2  invalid options or unavailable report setup

set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 2

JSON=0
CI_MODE=0
for arg in "$@"; do
    case "$arg" in
        --json) JSON=1 ;;
        --ci)   CI_MODE=1 ;;
        *) echo "unknown audit option: $arg" >&2; exit 2 ;;
    esac
done

PASS_COUNT=0
GAP_COUNT=0
SKIP_COUNT=0
ABS_COUNT=0
REQUIRED_GAPS=0
ROWS=()
TEST_TIMEOUT_SECS=${TV_AUDIT_TEST_TIMEOUT_SECS:-900}
case "$TEST_TIMEOUT_SECS" in
    ''|*[!0-9]*) echo "TV_AUDIT_TEST_TIMEOUT_SECS must be integer seconds" >&2; exit 2 ;;
esac
if [[ ${#TEST_TIMEOUT_SECS} -gt 4 ]]; then
    echo "TV_AUDIT_TEST_TIMEOUT_SECS must be between 1 and 3600" >&2
    exit 2
fi
TEST_TIMEOUT_SECS=$((10#$TEST_TIMEOUT_SECS))
if [[ "$TEST_TIMEOUT_SECS" -lt 1 || "$TEST_TIMEOUT_SECS" -gt 3600 ]]; then
    echo "TV_AUDIT_TEST_TIMEOUT_SECS must be between 1 and 3600" >&2
    exit 2
fi
AUDIT_LOG_DIR=$(mktemp -d "${TMPDIR:-/tmp}/tickvault-audit.XXXXXXXX") || exit 2
if [[ "$JSON" == 1 ]] && ! command -v jq >/dev/null 2>&1; then
    echo "jq is required for --json reporting" >&2
    exit 2
fi

c_green="\033[0;32m"
c_yellow="\033[0;33m"
c_red="\033[0;31m"
c_blue="\033[0;34m"
c_reset="\033[0m"

record() {
    # record <category> <status> <dimension> <proof>
    local cat="$1" status="$2" dim="$3" proof="$4"
    ROWS+=("${cat}|${status}|${dim}|${proof}")
    if [[ "$cat" == P || "$cat" == R ]] && [[ "$status" != PASS ]]; then
        REQUIRED_GAPS=$((REQUIRED_GAPS + 1))
    fi
    case "$status" in
        PASS) PASS_COUNT=$((PASS_COUNT + 1)) ;;
        GAP)  GAP_COUNT=$((GAP_COUNT + 1)) ;;
        SKIP) SKIP_COUNT=$((SKIP_COUNT + 1)) ;;
        ABS)  ABS_COUNT=$((ABS_COUNT + 1)) ;;
    esac
}

# -----------------------------------------------------------------------------
# Helper: run a check, record result
# -----------------------------------------------------------------------------
check_file_exists() {
    # check_file_exists <category> <dim> <path> <proof_description>
    local cat="$1" dim="$2" path="$3" proof="$4"
    if [[ -e "$path" ]]; then
        record "$cat" SKIP "$dim" "artifact exists; execution not verified: $path"
    else
        record "$cat" GAP "$dim" "missing: $path"
    fi
}

check_test_exists() {
    # check_test_exists <category> <dim> <crate> <test_file> <proof_description>
    local cat="$1" dim="$2" crate="$3" test_file="$4" proof="$5"
    if [[ -f "crates/${crate}/tests/${test_file}" ]] || [[ -f "crates/${crate}/src/${test_file}" ]]; then
        record "$cat" SKIP "$dim" "test source exists; not executed: $proof"
    else
        record "$cat" GAP "$dim" "missing test source: crates/${crate}/${test_file}"
    fi
}

check_cargo_test() {
    # check_cargo_test <category> <dim> <crate> <test_name> <proof>
    local cat="$1" dim="$2" crate="$3" test_name="$4" proof="$5"
    run_cargo_check "$cat" "$dim" "$proof" -p "$crate" --test "$test_name"
}

run_cargo_check() {
    local cat="$1" dim="$2" proof="$3"
    shift 3
    local log="$AUDIT_LOG_DIR/check-${#ROWS[@]}.log"
    if ! command -v cargo >/dev/null 2>&1 || ! command -v timeout >/dev/null 2>&1; then
        record "$cat" SKIP "$dim" "cargo/timeout unavailable; no test execution"
        return
    fi
    if timeout --kill-after=15s "$TEST_TIMEOUT_SECS" \
        cargo test --locked --offline "$@" --quiet -- --test-threads=1 > "$log" 2>&1; then
        if grep -Eq 'test result: ok\. [1-9][0-9]* passed; 0 failed;' "$log"; then
            record "$cat" PASS "$dim" "selected tests passed: $proof; log: $log"
        else
            record "$cat" GAP "$dim" "no nonzero passing test result; log: $log"
        fi
    else
        local code=$?
        record "$cat" GAP "$dim" "test build/execution failed or incomplete (exit $code); log: $log"
    fi
}

# =============================================================================
# COVERAGE + TESTING (P — mechanically provable)
# =============================================================================
check_file_exists P "Per-crate coverage floors (current coverage unmeasured)" \
    quality/crate-coverage-thresholds.toml \
    "quality/crate-coverage-thresholds.toml + scripts/coverage-gate.sh"

check_file_exists P "Mutation zero-survivors gate" \
    .github/workflows/mutation.yml \
    ".github/workflows/mutation.yml is a scheduled/main check; execution not verified"

check_file_exists P "Fuzz corpus" \
    fuzz/ \
    "fuzz/ dir + .github/workflows/fuzz.yml"

check_file_exists P "Sanitizers workflow (ASan + TSan)" \
    .github/workflows/safety.yml \
    ".github/workflows/safety.yml — weekly nightly ASan + TSan"

check_file_exists P "Bench budgets" \
    quality/benchmark-budgets.toml \
    "quality/benchmark-budgets.toml + scripts/bench-gate.sh (5% regression gate)"

check_file_exists P "22-test standard rule" \
    .claude/rules/project/testing.md \
    ".claude/rules/project/testing.md + scripts/validate-automation.sh"

# =============================================================================
# SOURCE QUALITY (P)
# =============================================================================
check_file_exists P "Banned-pattern scanner (6 categories)" \
    .claude/hooks/banned-pattern-scanner.sh \
    ".claude/hooks/banned-pattern-scanner.sh enforced by pre-commit + pre-push"

check_file_exists P "Pre-commit hook" \
    .claude/hooks/pre-commit-gate.sh \
    "fmt + clippy + banned + data integrity + secret + version + commit msg + typos (8 gates)"

check_file_exists P "Pre-push hook (12 gates)" \
    .claude/hooks/pre-push-gate.sh \
    ".claude/hooks/pre-push-gate.sh — 12 gates incl. test count ratchet"

check_file_exists P "cargo deny config (licenses + version pinning)" \
    deny.toml \
    "deny.toml — bans ^/~/*/>= and enforces license whitelist"

# =============================================================================
# PERFORMANCE (P)
# =============================================================================
check_file_exists P "Latency budgets (not an asymptotic proof)" \
    quality/benchmark-budgets.toml \
    "tick_parse ≤10ns, lookup ≤50ns, routing ≤100ns, full_tick ≤10μs"

check_file_exists P "Hot-path allocation budgets (DHAT execution unverified)" \
    crates/core/tests/dhat_allocation.rs \
    "crates/core/tests/{dhat_allocation,dhat_ws_reader_zero_alloc,dhat_deep_depth,dhat_token_handle,dhat_instrument_registry}.rs — hot-path 0-alloc via the dhat feature"

# =============================================================================
# OBSERVABILITY (P + R)
# =============================================================================
check_cargo_test P "ErrorCode tag on every error! site" \
    tickvault-common error_code_tag_guard \
    "crates/common/tests/error_code_tag_guard.rs"

check_cargo_test P "Every ErrorCode has runbook" \
    tickvault-common error_code_rule_file_crossref \
    "crates/common/tests/error_code_rule_file_crossref.rs"

check_cargo_test P "ErrorCode triage-rule source guard" \
    tickvault-common triage_rules_full_coverage_guard \
    "crates/common/tests/triage_rules_full_coverage_guard.rs (M2)"

check_cargo_test P "Metrics catalog no-drift" \
    tickvault-common metrics_catalog \
    "crates/common/tests/metrics_catalog.rs"

# 2026-06-14 de-stale: recording_rules_guard, resilience_sla_alert_guard and
# operator_health_dashboard_guard were DELETED in the CloudWatch-only migration
# (Prometheus/Alertmanager/Grafana removed, #O1/#O2/#O3). Their audit rows are
# removed so the 100% board reflects reality.
# 2026-07-18 (stage-4 dead-producer sweep): zero_tick_loss_alert_guard was
# DELETED with the tick rescue ring + TICK_BUFFER_CAPACITY (tick writer died
# in the stage-2 sweep 2026-07-17). Re-pointed to the LIVE absorption-tier
# ratchet: the seal-ring lib suite (SEAL_BUFFER_CAPACITY L-C1 lock, incl.
# test_seal_buffer_capacity_constant_is_locked_value). Inline because
# check_cargo_test only handles --test integration targets.
run_cargo_check P "Seal-ring capacity ratchet (SEAL_BUFFER_CAPACITY)" \
    "crates/trading/src/candles/seal_ring.rs (lib tests)" \
    -p tickvault-trading --lib candles::seal_ring

check_cargo_test P "Triage rules schema guard" \
    tickvault-common triage_rules_guard \
    "crates/common/tests/triage_rules_guard.rs"

check_cargo_test P "Error level meta-guard (WARN→ERROR)" \
    tickvault-storage error_level_meta_guard \
    "crates/storage/tests/error_level_meta_guard.rs"

# =============================================================================
# SECURITY (P + R)
# =============================================================================
check_file_exists P "cargo audit CI job" \
    .github/workflows/ci.yml \
    ".github/workflows/ci.yml Security & Audit job"

check_cargo_test P "API bearer auth middleware" \
    tickvault-api auth_middleware \
    "crates/api/tests/auth_middleware.rs (GAP-SEC-01)"

check_file_exists P "Secret scanner hook" \
    .claude/hooks/secret-scanner.sh \
    ".claude/hooks/secret-scanner.sh — pre-commit + pre-push"

check_file_exists P "Static IP verifier" \
    crates/core/src/network/ip_verifier.rs \
    "crates/core/src/network/ip_verifier.rs — pre-market check"

check_file_exists P "systemd unit hardening" \
    scripts/tv-tunnel/tickvault-tunnel.service \
    "NoNewPrivileges, ProtectSystem=strict, ProtectHome, PrivateTmp"

# =============================================================================
# DATA INTEGRITY + O(1) + UNIQUENESS + DEDUP (P)
# =============================================================================
check_cargo_test P "DEDUP segment meta-guard" \
    tickvault-storage dedup_segment_meta_guard \
    "crates/storage/tests/dedup_segment_meta_guard.rs (security_id + segment)"

check_cargo_test P "Live-feed purity (no backfill→ticks)" \
    tickvault-storage live_feed_purity_guard \
    "crates/storage/tests/live_feed_purity_guard.rs (6 tests)"

# 2026-06-14 de-stale: instrument_uniqueness_guard.rs was superseded by
# dedup_uniqueness_proptest.rs (the composite-key uniqueness property guard).
# Stage-2 dead-WS sweep (2026-07-17): dedup_uniqueness_proptest.rs RETIRED —
# its subjects (tick_persistence::{tick_dedup_key, tick_payload_hash}) were
# deleted with the dead Dhan tick chain. Composite-key dedup for the
# SURVIVING tables stays pinned by dedup_segment_meta_guard (checked above).

# =============================================================================
# AUTONOMOUS OPS (M1-M4, P)
# =============================================================================
check_cargo_test P "MCP endpoints config guard (M1)" \
    tickvault-common claude_mcp_endpoints_config_guard \
    "crates/common/tests/claude_mcp_endpoints_config_guard.rs"

check_cargo_test P "M3/M4 auto-fix + rollback + verify contract" \
    tickvault-common autonomous_ops_m3_m4_guard \
    "crates/common/tests/autonomous_ops_m3_m4_guard.rs (7 tests)"

check_file_exists P "Universal MCP config (committed)" \
    config/claude-mcp-endpoints.toml \
    "Branch-independent endpoints — every clone gets it"

check_file_exists P "Tunnel install scripts (Mac + AWS)" \
    scripts/tv-tunnel/install-mac.sh \
    "scripts/tv-tunnel/{install-mac,install-aws,doctor}.sh"

check_file_exists P "M4 verify harness" \
    scripts/triage/verify.sh \
    "Polls Prometheus for up to 120s; fail→rollback"

check_file_exists P "M4 rollback dispatcher" \
    scripts/triage/rollback.sh \
    "Looks up scripts/<fix>-rollback.sh by correlation_id"

# =============================================================================
# RUNTIME VERIFIABLE (R — requires live services, SKIP if sandbox)
# =============================================================================
# Prometheus/Alertmanager were retired. Their local ports cannot substantiate
# the current CloudWatch deployment. This command does not contact AWS.
record R SKIP "Live feed processing and loss alarms" \
    "current deployed revision, CloudWatch state and data reconciliation not inspected"

if command -v curl >/dev/null 2>&1 && curl -fsS -m 2 http://127.0.0.1:9000/ >/dev/null 2>&1; then
    record R PASS "QuestDB HTTP live" "http://127.0.0.1:9000/"
else
    record R SKIP "QuestDB HTTP" "local endpoint not reachable; deployment health unverified"
fi

# =============================================================================
# LAYERED ASYMPTOTIC (L — defense in depth, NOT absolute)
# =============================================================================
# 2026-07-18 truth-sync: the tv_ticks_dropped_total alert + tick rescue ring
# retired with the dead tick writer (stage-2/4 sweeps); the live defense is
# the candle-side seal chain.
record L SKIP "Data-loss defenses" \
    "buffering, spill and replay are design layers; this report does not execute a loss-reconciliation experiment"

record L SKIP "WebSocket recovery defenses" \
    "reconnect and watchdog behavior needs bounded fault tests and deployed observations"

record L SKIP "QuestDB recovery defenses" \
    "a responsive HTTP endpoint does not establish persistence or recovery correctness"

# =============================================================================
# IMPOSSIBLE ABSOLUTE (I — math forbids, closest proxies listed)
# =============================================================================
record I ABS "Zero bugs ever" \
    "finite tests cannot establish correctness for every unbounded input and external failure; formal proofs need explicit models and assumptions"

record I ABS "O(1) on ALL paths (hot + cold)" \
    "reading or emitting n records requires work proportional to n; DHAT, source scans and latency samples do not prove a universal asymptotic bound"

record I ABS "Absolute perfect security" \
    "Zero-day CVEs exist by definition. Closest proxies: cargo audit + cargo deny + secret scan + banned patterns + Secret<T>/zeroize + TLS (aws-lc-rs) + API auth middleware + systemd hardening + static IP + security-reviewer agent + weekly dependabot."

# =============================================================================
# EMIT REPORT
# =============================================================================
if [[ "$JSON" == "1" ]]; then
    printf '{\n  "timestamp":"%s",\n  "summary":{"pass":%d,"gap":%d,"skip":%d,"absolute":%d},\n  "rows":[\n' \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$PASS_COUNT" "$GAP_COUNT" "$SKIP_COUNT" "$ABS_COUNT"
    first=1
    for row in "${ROWS[@]}"; do
        IFS='|' read -r cat status dim proof <<< "$row"
        [[ $first -eq 0 ]] && printf ',\n'
        jq -cn --arg category "$cat" --arg status "$status" --arg dimension "$dim" --arg proof "$proof" \
            '{category:$category,status:$status,dimension:$dimension,proof:$proof}'
        first=0
    done
    printf '\n  ]\n}\n'
else
    echo "================================================================="
    echo "  tickvault Audit Evidence Tracker — $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "  Plan: .claude/plans/100pct-audit-tracker.md"
    echo "================================================================="
    printf "Categories: P=Mechanically Provable | R=Runtime | L=Layered | I=Impossible (closest proxy)\n\n"
    local_cat=""
    for row in "${ROWS[@]}"; do
        IFS='|' read -r cat status dim proof <<< "$row"
        if [[ "$cat" != "$local_cat" ]]; then
            case "$cat" in
                P) printf "\n${c_blue}--- P: Mechanically Provable ---${c_reset}\n" ;;
                R) printf "\n${c_blue}--- R: Runtime Verifiable ---${c_reset}\n" ;;
                L) printf "\n${c_blue}--- L: Layered Asymptotic (no absolute guarantee) ---${c_reset}\n" ;;
                I) printf "\n${c_blue}--- I: Impossible Absolute (closest proxies) ---${c_reset}\n" ;;
            esac
            local_cat="$cat"
        fi
        case "$status" in
            PASS) printf "  ${c_green}[PASS]${c_reset} %-55s  %s\n" "$dim" "$proof" ;;
            GAP)  printf "  ${c_red}[GAP ]${c_reset} %-55s  %s\n" "$dim" "$proof" ;;
            SKIP) printf "  ${c_yellow}[SKIP]${c_reset} %-55s  %s\n" "$dim" "$proof" ;;
            ABS)  printf "  ${c_yellow}[ABS ]${c_reset} %-55s  %s\n" "$dim" "$proof" ;;
        esac
    done
    echo
    echo "================================================================="
    printf "  PASS: ${c_green}%d${c_reset}   GAP: ${c_red}%d${c_reset}   SKIP: ${c_yellow}%d${c_reset}   ABSOLUTE-IMPOSSIBLE: ${c_yellow}%d${c_reset}\n" \
        "$PASS_COUNT" "$GAP_COUNT" "$SKIP_COUNT" "$ABS_COUNT"
    echo "================================================================="
    if [[ $REQUIRED_GAPS -eq 0 ]]; then
        printf "  ${c_green}Selected checks passed within their recorded scope.${c_reset}\n"
    else
        printf "  ${c_yellow}%d required dimension(s) failed or lack execution evidence.${c_reset}\n" "$REQUIRED_GAPS"
    fi
    echo "================================================================="
fi

# Both interactive and CI exit status distinguish missing evidence from PASS.
# L/I advisory rows never control the verdict.
if [[ $REQUIRED_GAPS -gt 0 ]]; then
    exit 1
fi
exit 0

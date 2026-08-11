#!/bin/bash
# Verify that 99-tickvault-net.conf ACTUALLY LANDED on this kernel.
#
# WHY THIS IS A SCRIPT AND NOT THREE LINES OF user-data
# -----------------------------------------------------
# user-data used to check exactly ONE value (rmem_max) and `echo` the verdict
# into /var/log/cloud-init-output.log. Two problems, both of the false-OK class:
#
#   1. A partial `sysctl --system` — one bad key, a read-only /proc entry, an
#      AMI that predates the file — could leave rmem_max correct while the
#      keepalive ladder, the softirq budget or max_map_count silently defaulted.
#      Checking one of nine values and reporting "APPLIED" is a false OK.
#   2. cloud-init-output.log is written once, at first boot, and read by nobody.
#      A warning nobody sees is a warning that does not exist.
#
# So: check EVERY load-bearing value, report to journald (which the CloudWatch
# agent ships, so it is visible off-box), leave a marker file the operator and
# the app can read long after boot, and exit non-zero so any caller that cares
# can react.
#
# DELIBERATELY DOES NOT HALT THE BOX. Same reasoning as crates/app/src/
# host_limits.rs: the app is useful without tuned buffers, and refusing to boot
# over a tuning miss trades a real outage for a theoretical one. This reports
# loudly and lets the operator decide. What it must never do is report success
# it has not verified.
set -uo pipefail

MARKER=/opt/tickvault/net-tuning.status
TAG=tickvault-net-tuning

# key<TAB>minimum. Every value here is load-bearing for the 16-WebSocket feed;
# see 99-tickvault-net.conf for the arithmetic behind each one.
# Kept in lockstep with that file by crates/app/tests/kernel_tuning_16ws_guard.rs.
CHECKS="
net.core.rmem_max	134217728
net.core.rmem_default	16777216
net.core.wmem_max	16777216
net.core.netdev_max_backlog	65536
net.core.netdev_budget	1200
net.core.somaxconn	4096
vm.max_map_count	1048576
vm.min_free_kbytes	262144
"

fail=0
report=""
while IFS=$'\t' read -r key want; do
    [ -z "${key:-}" ] && continue
    got="$(sysctl -n "$key" 2>/dev/null || echo missing)"
    if [ "$got" = missing ]; then
        report="$report
UNREADABLE $key (wanted >= $want)"
        fail=$((fail + 1))
    elif [ "$got" -lt "$want" ] 2>/dev/null; then
        report="$report
BELOW      $key = $got (wanted >= $want)"
        fail=$((fail + 1))
    else
        report="$report
ok         $key = $got"
    fi
done <<EOF
$CHECKS
EOF

# tcp_rmem is "min default max" — the scalar loop above cannot read it, and
# parsing its first field would report a 4 KB buffer as the ceiling. Pull the
# third field explicitly; this is the value that actually caps a busy socket.
tcp_rmem_max="$(sysctl -n net.ipv4.tcp_rmem 2>/dev/null | awk '{print $3}')"
if [ "${tcp_rmem_max:-0}" -ge 134217728 ] 2>/dev/null; then
    report="$report
ok         net.ipv4.tcp_rmem max = $tcp_rmem_max"
else
    report="$report
BELOW      net.ipv4.tcp_rmem max = ${tcp_rmem_max:-unreadable} (wanted >= 134217728)"
    fail=$((fail + 1))
fi

# Keepalive is the ONE setting where bigger is not better, so it cannot ride the
# ">= minimum" loop above: the stock kernel default is 7200 (2 hours), which
# would sail through a `>= 60` check while leaving the half-open backstop
# effectively disabled. It needs a WINDOW, checked at both ends.
ka_time="$(sysctl -n net.ipv4.tcp_keepalive_time 2>/dev/null || echo 0)"
ka_intvl="$(sysctl -n net.ipv4.tcp_keepalive_intvl 2>/dev/null || echo 0)"
ka_probes="$(sysctl -n net.ipv4.tcp_keepalive_probes 2>/dev/null || echo 0)"
ka_total=$((ka_time + ka_intvl * ka_probes))
if [ "$ka_time" -gt 60 ] 2>/dev/null || [ "$ka_time" -le 0 ] 2>/dev/null; then
    # Too slow (or unreadable): a dead peer is never detected by the backstop.
    report="$report
BELOW      net.ipv4.tcp_keepalive_time = $ka_time (wanted <= 60; stock 7200 disables the backstop)"
    fail=$((fail + 1))
elif [ "$ka_total" -le 40 ] 2>/dev/null; then
    # Too fast: keepalive would tear down sockets ahead of Dhan's own 40 s ping
    # deadline and the app's 27 s idle watchdog, turning a recoverable stall
    # into a reconnect storm across sixteen connections at once.
    report="$report
BELOW      keepalive ladder = ${ka_total}s (must exceed Dhan's 40 s ping deadline)"
    fail=$((fail + 1))
else
    report="$report
ok         keepalive ladder = ${ka_total}s (fires after Dhan's 40 s, after the app's 27 s)"
fi

if [ "$fail" -eq 0 ]; then
    verdict="APPLIED — all kernel tuning verified for the 16-WebSocket feed"
else
    verdict="NOT APPLIED — $fail setting(s) missing or below target. The kernel \
may discard market data under load and no downstream buffer can recover it. \
Re-run: cp /opt/tickvault/repo/deploy/aws/sysctl/99-tickvault-net.conf \
/etc/sysctl.d/ && sysctl --system"
fi

mkdir -p "$(dirname "$MARKER")" 2>/dev/null || true
printf '%s: %s\n%s\n' "$(date -u +%FT%TZ)" "$verdict" "$report" >"$MARKER" 2>/dev/null || true

# journald so it ships off-box; stdout so it also lands in cloud-init's log.
logger -t "$TAG" -p daemon.err "$verdict" 2>/dev/null || true
echo "$TAG: $verdict"
echo "$report"

exit "$([ "$fail" -eq 0 ] && echo 0 || echo 1)"

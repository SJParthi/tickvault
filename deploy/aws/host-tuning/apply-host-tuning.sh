#!/usr/bin/env bash
# Host tuning that is NOT sysctl — applied at every boot.
#
# Why this is a file and not inline in user-data.sh.tftpl: EC2 caps user-data
# at 16384 bytes, and `crates/common/tests/user_data_size_guard.rs` enforces a
# 512-byte margin under that. The template sat at 16382 bytes on 2026-08-10 —
# two bytes spare — and an inline block pushed a terraform PLAN over the hard
# limit, failing every unrelated resource in the same run. Anything that can
# wait until after the Step 5 repo clone belongs here. All of these can.
#
# Companion: 99-tickvault-net.conf (the sysctl half) + verify-net-tuning.sh.
#
# Exit code is deliberately always 0. Neither setting is required for the app
# to run correctly, and a non-zero exit here would abort user-data and take the
# whole boot with it. Failures are LOUD in journald instead.

set -uo pipefail

# ---- Transparent hugepages -> madvise ----
#
# NOT a sysctl — a /sys write, so it must be re-applied on every boot.
#
# `madvise` is chosen DELIBERATELY over `never`. The usual latency advice is
# "disable THP", and that advice is aimed at a machine running one latency
# process. This box also runs QuestDB, which benefits from hugepages for its
# column memory; `never` would take that away to solve a problem the feed path
# does not have (it allocates almost nothing steady-state — see the DHAT
# gates). `madvise` gives THP only to code that explicitly asks, which serves
# both tenants instead of trading one for the other.
if [ -w /sys/kernel/mm/transparent_hugepage/enabled ]; then
    echo madvise > /sys/kernel/mm/transparent_hugepage/enabled 2>/dev/null || true
    echo madvise > /sys/kernel/mm/transparent_hugepage/defrag 2>/dev/null || true
    echo "host-tuning: THP = $(cat /sys/kernel/mm/transparent_hugepage/enabled 2>/dev/null || echo unreadable)"
else
    echo "host-tuning: WARNING transparent_hugepage not writable — left at distro default"
fi

# ---- Assert the clock is disciplined ----
#
# Every per-connection latency number this system publishes is computed as
# (our receive instant - the exchange timestamp). If this host's clock is not
# synchronised, that subtraction measures CLOCK SKEW and reports it as feed
# latency — and a wrong number is worse than no number, because it looks
# actionable. AL2023 ships chrony pointed at the Amazon Time Sync service by
# default, but "ships with it by default" is an assumption, and nothing here
# had ever verified it. This makes it visible either way.
if command -v chronyc >/dev/null 2>&1; then
    systemctl enable --now chronyd 2>/dev/null || true
    # A fresh boot has not necessarily stepped yet; give it a moment to reach
    # a source before asking.
    sleep 2
    if chronyc tracking >/dev/null 2>&1; then
        echo "host-tuning: clock OK — $(chronyc tracking 2>/dev/null | awk -F': ' '/Reference ID|System time/{printf "%s; ", $2}')"
    else
        echo "host-tuning: WARNING chronyd present but not yet tracking a source — latency numbers may be skewed"
    fi
else
    echo "host-tuning: WARNING chrony absent — host clock is UNDISCIPLINED and every latency metric is untrustworthy"
fi


# ---- NIC IRQ steering + app CPU-confinement safety ----
#
# Operator Quote 17b, 2026-08-19 (CPU isolation). Both halves below are /proc
# and /etc writes, so like THP they do not survive a reboot and belong here
# rather than in user-data (which runs once per INSTANCE, and which is 116
# bytes from the 16 KiB EC2 hard limit anyway).
#
# THE PARTITION this completes (other halves: docker-compose.yml for QuestDB
# and the sidecars, tickvault.service for the app):
#
#   core 0  OS + NIC IRQ/softirq + log sidecars   <- steered here, below
#   core 1  tickvault app — exclusive
#   core 2  shared burst: app + QuestDB
#   core 3  QuestDB — exclusive
#
# Core 0 is chosen for the IRQs precisely BECAUSE it is not a valid pin target
# for the decoder: CLAUDE.md records that pinning the drain onto the softirq
# core puts it in contention with the work that feeds it. Concentrating the
# interrupts on the one core no latency-critical thread is allowed to occupy
# is the same rule read from the other end.
#
# Everything here is best-effort and idempotent: rewriting an affinity mask
# that already holds the value is a no-op, and every failure is a logged
# WARNING, never a non-zero exit. A host with default IRQ placement is slower;
# a host whose boot aborted has no app at all.
#
# ROLLBACK (no code change): put `TV_IRQ_STEER=off` in
# /etc/tickvault/host-tuning.env and reboot, or simply re-enable irqbalance —
# `systemctl enable --now irqbalance` re-spreads the interrupts immediately.
TV_IRQ_STEER="${TV_IRQ_STEER:-on}"
TV_IRQ_CPU="${TV_IRQ_CPU:-0}"
TV_APP_CPUS_MIN_CORES=3

if [ "$TV_IRQ_STEER" = "off" ]; then
    echo "host-tuning: IRQ steering DISABLED by TV_IRQ_STEER=off — NIC interrupts left to the kernel/irqbalance"
else
    # irqbalance actively re-spreads interrupts on a timer. Leaving it running
    # while writing static masks would produce a tuning that LOOKS applied and
    # silently reverts minutes later — the false-OK class, so it is stopped
    # explicitly and the fact is logged either way.
    if systemctl is-active --quiet irqbalance 2>/dev/null; then
        systemctl disable --now irqbalance >/dev/null 2>&1 \
            && echo "host-tuning: irqbalance STOPPED — static IRQ affinity would otherwise be re-spread on its timer" \
            || echo "host-tuning: WARNING could not stop irqbalance — static IRQ affinity may be undone within minutes"
    else
        echo "host-tuning: irqbalance not running — static IRQ affinity will stick"
    fi

    # Resolve the NIC from the default route rather than hardcoding a name:
    # the interface is ens34 on this instance, but that is an instance
    # property, not a repo constant, and a wrong name would silently steer
    # nothing.
    TV_NIC="$(ip -o route show default 2>/dev/null | awk '{for(i=1;i<=NF;i++) if($i=="dev"){print $(i+1); exit}}')"
    if [ -z "${TV_NIC:-}" ]; then
        echo "host-tuning: WARNING could not resolve the default-route interface — NIC IRQs NOT steered"
    else
        tv_irq_moved=0
        tv_irq_failed=0
        # /proc/interrupts names ENA vectors after the interface. Match on the
        # interface name so combined/Tx-Rx queue vectors are all caught.
        for tv_irq in $(awk -v nic="$TV_NIC" '$NF ~ nic || $0 ~ nic {sub(":","",$1); if ($1 ~ /^[0-9]+$/) print $1}' /proc/interrupts 2>/dev/null); do
            if [ -w "/proc/irq/${tv_irq}/smp_affinity_list" ]; then
                if echo "$TV_IRQ_CPU" > "/proc/irq/${tv_irq}/smp_affinity_list" 2>/dev/null; then
                    tv_irq_moved=$((tv_irq_moved + 1))
                else
                    # Some vectors are managed by the driver and refuse writes.
                    # That is expected, not an error worth failing a boot over.
                    tv_irq_failed=$((tv_irq_failed + 1))
                fi
            else
                tv_irq_failed=$((tv_irq_failed + 1))
            fi
        done
        echo "host-tuning: NIC ${TV_NIC} IRQs -> cpu ${TV_IRQ_CPU} (moved=${tv_irq_moved} refused=${tv_irq_failed})"
        if [ "$tv_irq_moved" -eq 0 ]; then
            echo "host-tuning: WARNING no NIC IRQ was steered — softirq may still land on the app's core"
        fi
        # RPS is deliberately LEFT DISABLED (rps_cpus stays 0). With RPS off,
        # softirq processing happens on the core that took the interrupt —
        # which is now core 0 by construction. Turning RPS on would re-spread
        # that processing back across the cores this change just cleared.
    fi
fi

# ---- App CPU-confinement safety net ----
#
# tickvault.service carries `AllowedCPUs=1-2`, which needs at least 3 cores.
# On a smaller host that set is unsatisfiable, and the honest response is to
# run the app UNCONFINED rather than to pin it into a set the kernel cannot
# honour. This writes (or removes) a drop-in BEFORE the app starts — the unit
# is ordered Before=tickvault.service, so the reload below always lands first.
TV_DROPIN_DIR=/etc/systemd/system/tickvault.service.d
TV_DROPIN=${TV_DROPIN_DIR}/90-cpu-guard.conf
tv_cores="$(nproc 2>/dev/null || echo 0)"
if [ "$tv_cores" -lt "$TV_APP_CPUS_MIN_CORES" ]; then
    mkdir -p "$TV_DROPIN_DIR" 2>/dev/null || true
    tv_want="[Service]
# Written by apply-host-tuning.sh: this host has ${tv_cores} core(s), fewer than
# the ${TV_APP_CPUS_MIN_CORES} the AllowedCPUs=1-2 partition needs. Empty assignment resets it.
AllowedCPUs=
"
    if [ ! -f "$TV_DROPIN" ] || [ "$(cat "$TV_DROPIN" 2>/dev/null)" != "$tv_want" ]; then
        printf '%s' "$tv_want" > "$TV_DROPIN" 2>/dev/null \
            && systemctl daemon-reload 2>/dev/null || true
    fi
    echo "host-tuning: WARNING only ${tv_cores} core(s) — app CPU confinement REMOVED (needs >= ${TV_APP_CPUS_MIN_CORES})"
elif [ -f "$TV_DROPIN" ]; then
    # The host grew back. Remove the escape hatch so the partition applies.
    rm -f "$TV_DROPIN" 2>/dev/null && systemctl daemon-reload 2>/dev/null || true
    echo "host-tuning: ${tv_cores} cores — removed the CPU-confinement escape hatch; AllowedCPUs=1-2 now applies"
else
    echo "host-tuning: ${tv_cores} cores — app confined to CPUs 1-2, QuestDB to 2-3, IRQs on cpu ${TV_IRQ_CPU}"
fi

# ---- App memory-throttle safety net ----
#
# tickvault.service carries a fixed `MemoryHigh=`, tuned against the locked
# r8g.xlarge (32 GiB). On a SMALLER host that number can exceed physical RAM,
# and then it is not merely generous -- it is INERT, and it takes two other
# safety nets with it. With no `MemoryMax=`, `resolve_memory_ceiling` resolves
# the app's ceiling FROM `memory.high`, so the WAL catch-up stand-down (60% of
# it) and the RESOURCE-02 page line (80%) are both derived from a number the
# host can never reach. All three would sit silently disabled while every
# surface reported health -- the "alarm that cannot fire" class.
#
# The condition is deliberately NARROW: act only when the repo value is at or
# above MemTotal. The operator's value intentionally OVERCOMMITS -- MemoryHigh
# is a throttle, not a reservation, and 15G was measured re-entering a SIGABRT
# loop -- so a formula that "budgeted" the app against QuestDB's share would
# LOWER the ceiling on the locked host and move the two derived thresholds with
# it. On any host where the repo value is reachable, no drop-in is written and
# behaviour is byte-identical to the unit.
#
# The repo value is READ from the unit, never restated here. A second copy of
# that number is drift waiting to happen, and this file would be the copy
# nobody updates.
TV_MEM_DROPIN=${TV_DROPIN_DIR}/91-memory-guard.conf
tv_unit_high_g="$(sed -n 's/^MemoryHigh=\([0-9]\+\)G$/\1/p' \
    /etc/systemd/system/tickvault.service 2>/dev/null | tail -1)"
tv_memkb="$(sed -n 's/^MemTotal:[[:space:]]*\([0-9]\+\) kB$/\1/p' /proc/meminfo 2>/dev/null | head -1)"
tv_mem_g=$(( ${tv_memkb:-0} / 1048576 ))

if [ -z "$tv_unit_high_g" ] || [ "$tv_mem_g" -le 0 ]; then
    # Cannot read one of the two inputs. Change NOTHING -- a derived ceiling
    # from a guessed input is worse than the unit's own value.
    echo "host-tuning: memory guard SKIPPED (unit MemoryHigh='${tv_unit_high_g:-unreadable}', MemTotal=${tv_mem_g}G)"
elif [ "$tv_unit_high_g" -lt "$tv_mem_g" ]; then
    if [ -f "$TV_MEM_DROPIN" ]; then
        # The host grew back. Remove the escape hatch so the unit's own value
        # applies again, along with the thresholds derived from it.
        rm -f "$TV_MEM_DROPIN" 2>/dev/null && systemctl daemon-reload 2>/dev/null || true
        echo "host-tuning: ${tv_mem_g}G host — removed the memory escape hatch; MemoryHigh=${tv_unit_high_g}G now applies"
    else
        echo "host-tuning: ${tv_mem_g}G host — MemoryHigh=${tv_unit_high_g}G is reachable, no drop-in needed"
    fi
else
    # QuestDB's share uses the SAME formula deploy-aws.yml writes into
    # QDB_MEM_LIMIT (4/10 of MemTotal, floor 1, cap 12). Reproducing it here
    # rather than inventing a second one is the point: two derivations that
    # disagree would hand the same RAM to both tenants.
    tv_qdb_g=$(( tv_mem_g * 4 / 10 ))
    [ "$tv_qdb_g" -lt 1 ] && tv_qdb_g=1
    [ "$tv_qdb_g" -gt 12 ] && tv_qdb_g=12
    tv_want_g=$(( tv_mem_g - tv_qdb_g - 1 ))   # -1G: OS + page-cache floor
    [ "$tv_want_g" -lt 1 ] && tv_want_g=1
    mkdir -p "$TV_DROPIN_DIR" 2>/dev/null || true
    tv_mem_want="[Service]
# Written by apply-host-tuning.sh: this host has ${tv_mem_g}G, so the unit's
# MemoryHigh=${tv_unit_high_g}G can never be reached -- the throttle, the WAL
# catch-up stand-down and the RESOURCE-02 page line would all be inert.
# ${tv_mem_g}G total - ${tv_qdb_g}G QuestDB (the deploy formula) - 1G OS floor.
MemoryHigh=${tv_want_g}G
"
    if [ ! -f "$TV_MEM_DROPIN" ] || [ "$(cat "$TV_MEM_DROPIN" 2>/dev/null)" != "$tv_mem_want" ]; then
        printf '%s' "$tv_mem_want" > "$TV_MEM_DROPIN" 2>/dev/null \
            && systemctl daemon-reload 2>/dev/null || true
    fi
    echo "host-tuning: WARNING ${tv_mem_g}G host — MemoryHigh lowered ${tv_unit_high_g}G -> ${tv_want_g}G so the throttle and its derived alarms can fire"
fi

exit 0

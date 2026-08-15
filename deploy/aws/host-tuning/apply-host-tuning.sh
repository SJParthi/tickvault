#!/usr/bin/env bash
# Host tuning that is NOT sysctl — applied at every boot.
#
# Why this is a file and not inline in user-data.sh.tftpl: EC2 caps user-data
# at 16384 bytes, and `crates/common/tests/user_data_size_guard.rs` enforces a
# 512-byte margin under that. The template sat at 16382 bytes on 2026-08-10 —
# two bytes spare — and an inline block pushed a terraform PLAN over the hard
# limit, failing every unrelated resource in the same run. Anything that can
# wait until after the Step 5 repo clone belongs here. These two can.
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

exit 0

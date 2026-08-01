#!/usr/bin/env bash
# Install the AWS CLI if it is missing — OFFICIAL installer, zero interpreters.
#
# 2026-08-01 (operator directive — Rust-O(1)-only workspace): replaces the
# `pip3 install awscli` fallback that lived in bootstrap.sh /
# provision-infra-secrets.sh / setup-secrets.sh. That path pulled the AWS CLI
# **v1** wheel from PyPI and required an interpreter + its package manager on the host —
# a live interpreted-language dependency in the setup chain, and one the
# rust_only_guard did not catch (its token set covers the runtime name, never `pip`).
#
# The official installer ships a SELF-CONTAINED binary (its own embedded
# runtime, nothing to install alongside it) and gives v2 instead of the
# long-deprecated v1. Verified reachable 2026-08-01:
#   curl -sS -o /dev/null -w '%{http_code}' \
#     "https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip"  -> 200 (73 MB)
#
# Single source of truth: every script that needs the AWS CLI calls THIS file.
# Idempotent — a no-op when `aws` already resolves.
set -euo pipefail

if command -v aws > /dev/null 2>&1; then
    exit 0
fi

echo "AWS CLI not found — installing (official installer, no interpreter)..." >&2

# Prefer sudo only when we are not already root; a dev box without sudo still
# gets a clear failure rather than a silent partial install.
SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    if command -v sudo > /dev/null 2>&1; then
        SUDO="sudo"
    else
        echo "ERROR: need root (or sudo) to install the AWS CLI." >&2
        exit 1
    fi
fi

WORK_DIR="$(mktemp -d)"
# Clean up the ~73 MB download whatever happens.
trap 'rm -rf "${WORK_DIR}"' EXIT

case "$(uname -s)" in
    Darwin)
        curl -fsSL "https://awscli.amazonaws.com/AWSCLIV2.pkg" \
            -o "${WORK_DIR}/AWSCLIV2.pkg"
        ${SUDO} installer -pkg "${WORK_DIR}/AWSCLIV2.pkg" -target /
        ;;
    Linux)
        # uname -m gives x86_64 / aarch64 — both are published names.
        curl -fsSL "https://awscli.amazonaws.com/awscli-exe-linux-$(uname -m).zip" \
            -o "${WORK_DIR}/awscliv2.zip"
        if ! command -v unzip > /dev/null 2>&1; then
            echo "ERROR: 'unzip' is required to install the AWS CLI." >&2
            exit 1
        fi
        unzip -q "${WORK_DIR}/awscliv2.zip" -d "${WORK_DIR}"
        ${SUDO} "${WORK_DIR}/aws/install" --update
        ;;
    *)
        echo "ERROR: unsupported OS '$(uname -s)' for automatic AWS CLI install." >&2
        exit 1
        ;;
esac

if ! command -v aws > /dev/null 2>&1; then
    echo "ERROR: AWS CLI install completed but 'aws' is still not on PATH." >&2
    exit 1
fi

echo "AWS CLI installed: $(aws --version 2>&1)" >&2

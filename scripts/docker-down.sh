#!/usr/bin/env bash
# =============================================================================
# dhan-live-trader — Docker Down
# =============================================================================
# Stops all dlt-* containers. Works with or without SSM env vars in the shell.
#
# docker compose needs env vars set to PARSE the compose file (even for down).
# These placeholders are never used by any container — just satisfies the YAML parser.
# =============================================================================

set -euo pipefail

export DLT_QUESTDB_PG_USER="${DLT_QUESTDB_PG_USER:-_teardown}"
export DLT_QUESTDB_PG_PASSWORD="${DLT_QUESTDB_PG_PASSWORD:-_teardown}"
export DLT_GRAFANA_ADMIN_USER="${DLT_GRAFANA_ADMIN_USER:-_teardown}"
export DLT_GRAFANA_ADMIN_PASSWORD="${DLT_GRAFANA_ADMIN_PASSWORD:-_teardown}"

docker compose -f deploy/docker/docker-compose.yml down --remove-orphans
echo ""
echo "All dlt-* containers stopped."

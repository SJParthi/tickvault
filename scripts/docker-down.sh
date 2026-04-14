#!/usr/bin/env bash
# =============================================================================
# tickvault — Docker Down
# =============================================================================
# Stops all tv-* containers. Works with or without SSM env vars in the shell.
#
# docker compose needs env vars set to PARSE the compose file (even for down).
# These placeholders are never used by any container — just satisfies the YAML parser.
# =============================================================================

set -euo pipefail

export TV_QUESTDB_PG_USER="${TV_QUESTDB_PG_USER:-_teardown}"
export TV_QUESTDB_PG_PASSWORD="${TV_QUESTDB_PG_PASSWORD:-_teardown}"
export TV_GRAFANA_ADMIN_USER="${TV_GRAFANA_ADMIN_USER:-_teardown}"
export TV_GRAFANA_ADMIN_PASSWORD="${TV_GRAFANA_ADMIN_PASSWORD:-_teardown}"

docker compose -f deploy/docker/docker-compose.yml down --remove-orphans
echo ""
echo "All tv-* containers stopped."

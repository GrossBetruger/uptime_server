#!/usr/bin/env bash
set -e

# Configuration (should match run_postgres.sh)
CONTAINER_NAME="${CONTAINER_NAME:-uptime-postgres}"
POSTGRES_DB="${POSTGRES_DB:-uptime_db}"
POSTGRES_USER="${POSTGRES_USER:-uptime_user}"
POSTGRES_PASSWORD="${POSTGRES_PASSWORD:-uptime_password}"
POSTGRES_PORT="${POSTGRES_PORT:-5432}"

# Method 1: Using docker exec (works even if psql is not installed on host)
echo "Querying database using docker exec..."
docker exec -it "$CONTAINER_NAME" psql -U "$POSTGRES_USER" -d "$POSTGRES_DB" -c "SELECT * FROM uptime_logs;"



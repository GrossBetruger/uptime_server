#!/usr/bin/env bash
set -e

# Configuration (should match run_postgres.sh)
CONTAINER_NAME="${CONTAINER_NAME:-uptime-postgres}"
POSTGRES_DB="${POSTGRES_DB:-uptime_db}"
POSTGRES_USER="${POSTGRES_USER:-uptime_user}"
BACKUP_DIR="${BACKUP_DIR:-backups}"

# Create backup directory if it doesn't exist
mkdir -p "$BACKUP_DIR"

# Generate timestamp for backup filename
TIMESTAMP=$(date +"%Y%m%d_%H%M%S")
BACKUP_FILE="${BACKUP_DIR}/uptime_db_${TIMESTAMP}.sql"

echo "Backing up database '$POSTGRES_DB' from container '$CONTAINER_NAME'..."
echo "Backup will be saved to: $BACKUP_FILE"

# Check if container is running
if ! docker ps --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"; then
    echo "Error: Container '$CONTAINER_NAME' is not running!"
    exit 1
fi

# Perform backup using pg_dump via docker exec
docker exec "$CONTAINER_NAME" pg_dump -U "$POSTGRES_USER" -d "$POSTGRES_DB" > "$BACKUP_FILE"

# Check if backup was successful
if [ $? -eq 0 ]; then
    BACKUP_SIZE=$(du -h "$BACKUP_FILE" | cut -f1)
    echo "Backup completed successfully!"
    echo "Backup file: $BACKUP_FILE"
    echo "Backup size: $BACKUP_SIZE"
else
    echo "Error: Backup failed!"
    rm -f "$BACKUP_FILE"
    exit 1
fi


#!/usr/bin/env bash
set -e

# Configuration
CONTAINER_NAME="${CONTAINER_NAME:-uptime-postgres}"
POSTGRES_DB="${POSTGRES_DB:-uptime_db}"
POSTGRES_USER="${POSTGRES_USER:-uptime_user}"
POSTGRES_PASSWORD="${POSTGRES_PASSWORD:-uptime_password}"
BACKUP_FILE="backups/test_backup.sql"
TEST_CONTAINER_NAME="${CONTAINER_NAME}-test"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo "=== Database Backup Restore Test ==="
echo ""

# Check if backup file exists
if [ ! -f "$BACKUP_FILE" ]; then
    echo -e "${RED}Error: Backup file '$BACKUP_FILE' not found!${NC}"
    exit 1
fi

echo "✓ Backup file found: $BACKUP_FILE"

# Check if test container exists, if not create it
if ! docker ps -a --format '{{.Names}}' | grep -q "^${TEST_CONTAINER_NAME}$"; then
    echo "Creating test container '$TEST_CONTAINER_NAME'..."
    
    # Get the image name from the existing container or use default
    if docker ps -a --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"; then
        IMAGE_NAME=$(docker inspect "$CONTAINER_NAME" --format '{{.Config.Image}}')
    else
        IMAGE_NAME="uptime-postgres:latest"
    fi
    
    # Create a temporary test container
    docker run -d \
        --name "$TEST_CONTAINER_NAME" \
        -e POSTGRES_DB="$POSTGRES_DB" \
        -e POSTGRES_USER="$POSTGRES_USER" \
        -e POSTGRES_PASSWORD="$POSTGRES_PASSWORD" \
        "$IMAGE_NAME" > /dev/null
    
    # Wait for PostgreSQL to be ready
    echo "Waiting for PostgreSQL to be ready..."
    sleep 3
    max_attempts=30
    attempt=0
    while [ $attempt -lt $max_attempts ]; do
        if docker exec "$TEST_CONTAINER_NAME" pg_isready -U "$POSTGRES_USER" > /dev/null 2>&1; then
            break
        fi
        attempt=$((attempt + 1))
        sleep 1
    done
    
    if [ $attempt -eq $max_attempts ]; then
        echo -e "${RED}Error: PostgreSQL container failed to start${NC}"
        docker rm -f "$TEST_CONTAINER_NAME" > /dev/null 2>&1
        exit 1
    fi
    echo "✓ Test container created and ready"
else
    echo "Test container '$TEST_CONTAINER_NAME' already exists"
    
    # Check if container is running
    if ! docker ps --format '{{.Names}}' | grep -q "^${TEST_CONTAINER_NAME}$"; then
        echo "Starting existing test container..."
        docker start "$TEST_CONTAINER_NAME" > /dev/null
        sleep 2
    fi
fi

# Drop existing database if it exists and recreate
echo "Preparing database..."
docker exec "$TEST_CONTAINER_NAME" psql -U "$POSTGRES_USER" -d postgres -c "DROP DATABASE IF EXISTS $POSTGRES_DB;" > /dev/null 2>&1 || true
docker exec "$TEST_CONTAINER_NAME" psql -U "$POSTGRES_USER" -d postgres -c "CREATE DATABASE $POSTGRES_DB;" > /dev/null 2>&1
echo "✓ Database prepared"

# Restore backup file
echo "Restoring backup file..."
if docker exec -i "$TEST_CONTAINER_NAME" psql -U "$POSTGRES_USER" -d "$POSTGRES_DB" < "$BACKUP_FILE" > /dev/null 2>&1; then
    echo "✓ Backup restored successfully"
else
    echo -e "${RED}Error: Failed to restore backup${NC}"
    docker rm -f "$TEST_CONTAINER_NAME" > /dev/null 2>&1
    exit 1
fi

# Perform SELECT * query
echo ""
echo "=== Querying restored data ==="
echo ""
docker exec "$TEST_CONTAINER_NAME" psql -U "$POSTGRES_USER" -d "$POSTGRES_DB" -c "SELECT * FROM uptime_logs;"

# Get row count
ROW_COUNT=$(docker exec "$TEST_CONTAINER_NAME" psql -U "$POSTGRES_USER" -d "$POSTGRES_DB" -t -c "SELECT COUNT(*) FROM uptime_logs;" | tr -d ' ')

echo ""
echo -e "${GREEN}✓ Test completed successfully!${NC}"
echo "  Total rows restored: $ROW_COUNT"

# Optionally clean up test container
if [ "${CLEANUP_TEST_CONTAINER:-true}" = "true" ]; then
    echo ""
    echo "Cleaning up test container..."
    docker rm -f "$TEST_CONTAINER_NAME" > /dev/null 2>&1
    echo "✓ Test container removed"
fi

echo ""
echo "=== Test Summary ==="
echo -e "${GREEN}✓ Backup file found${NC}"
echo -e "${GREEN}✓ Backup restored successfully${NC}"
echo -e "${GREEN}✓ Data verified with SELECT *${NC}"
echo -e "${GREEN}✓ Row count: $ROW_COUNT${NC}"


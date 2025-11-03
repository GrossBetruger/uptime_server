#!/usr/bin/env bash
set -e

# Configuration
IMAGE_NAME="uptime-postgres"
CONTAINER_NAME="uptime-postgres"
VOLUME_NAME="uptime-postgres-data"
POSTGRES_PORT="${POSTGRES_PORT:-5432}"
POSTGRES_DB="${POSTGRES_DB:-uptime_db}"
POSTGRES_USER="${POSTGRES_USER:-uptime_user}"
POSTGRES_PASSWORD="${POSTGRES_PASSWORD:-uptime_password}"

# Build the image if it doesn't exist
if ! docker image inspect "$IMAGE_NAME:latest" >/dev/null 2>&1; then
    echo "Building Docker image..."
    docker build -t "$IMAGE_NAME:latest" -f docker/postgres/Dockerfile docker/postgres
fi

# Stop and remove existing container if it exists
if docker ps -a --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"; then
    echo "Stopping existing container..."
    docker stop "$CONTAINER_NAME" >/dev/null 2>&1 || true
    docker rm "$CONTAINER_NAME" >/dev/null 2>&1 || true
fi

# Create volume if it doesn't exist
if ! docker volume inspect "$VOLUME_NAME" >/dev/null 2>&1; then
    echo "Creating persistent volume..."
    docker volume create "$VOLUME_NAME"
fi

# Run the container
echo "Starting PostgreSQL container..."
docker run -d \
    --name "$CONTAINER_NAME" \
    -p "${POSTGRES_PORT}:5432" \
    -e POSTGRES_DB="$POSTGRES_DB" \
    -e POSTGRES_USER="$POSTGRES_USER" \
    -e POSTGRES_PASSWORD="$POSTGRES_PASSWORD" \
    -v "$VOLUME_NAME:/var/lib/postgresql/data" \
    "$IMAGE_NAME:latest"

echo "PostgreSQL container started!"
echo "Connection string: postgresql://${POSTGRES_USER}:${POSTGRES_PASSWORD}@localhost:${POSTGRES_PORT}/${POSTGRES_DB}"
echo "Data is persisted in Docker volume: $VOLUME_NAME"


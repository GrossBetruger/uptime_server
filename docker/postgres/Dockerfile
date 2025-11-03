FROM postgres:16-alpine

# Copy initialization SQL script
COPY init.sql /docker-entrypoint-initdb.d/

# Set default environment variables (can be overridden)
ENV POSTGRES_DB=uptime_db
ENV POSTGRES_USER=uptime_user
ENV POSTGRES_PASSWORD=uptime_password

# Expose PostgreSQL port
EXPOSE 5432


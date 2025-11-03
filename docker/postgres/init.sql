-- PostgreSQL schema for uptime monitor data

-- This schema stores the data from the "line" format:

-- unix iso user_name public_ip isn_info status_text



-- Create the uptime_logs table

CREATE TABLE IF NOT EXISTS uptime_logs (

    id BIGSERIAL PRIMARY KEY,

    unix_timestamp BIGINT NOT NULL,

    iso_timestamp TIMESTAMPTZ NOT NULL,

    user_name VARCHAR(50) NOT NULL,

    public_ip INET NOT NULL,

    isn_info VARCHAR(255),

    status TEXT NOT NULL CHECK (status IN ('online', 'offline')),

    created_at TIMESTAMPTZ DEFAULT NOW()

);



-- Create indexes for common query patterns

CREATE INDEX IF NOT EXISTS idx_uptime_logs_unix_timestamp ON uptime_logs(unix_timestamp);

CREATE INDEX IF NOT EXISTS idx_uptime_logs_user_name ON uptime_logs(user_name);

CREATE INDEX IF NOT EXISTS idx_uptime_logs_iso_timestamp ON uptime_logs(iso_timestamp);


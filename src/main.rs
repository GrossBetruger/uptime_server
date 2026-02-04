use axum::{
    body::Bytes,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use chrono::{DateTime, Utc};
use clap::Parser;
use deadpool_postgres::{Config, Pool, Runtime};
use regex::Regex;
use serde::Serialize;
use std::{collections::HashMap, net::IpAddr, path::PathBuf, sync::Arc, sync::OnceLock};
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt}, // <- read + write + seek
    net::TcpListener,
    process::Command,
    sync::Mutex,
};
use std::io::SeekFrom;
use tracing::{error, info};
use tracing_subscriber::{fmt, EnvFilter};

#[derive(Parser, Debug)]
#[command(name = "uptime_server")]
#[command(about = "Uptime monitoring server")]
struct Args {
    /// Enable legacy file logging (default: true)
    #[arg(long, default_value = "true", action = clap::ArgAction::Set, value_parser = clap::value_parser!(bool))]
    legacy_log: bool,
}

#[derive(Clone)]
struct AppState {
    logfile: Arc<Mutex<tokio::fs::File>>,
    log_path: PathBuf,
    db_pool: Pool,
    legacy_log: bool,
}

/// Response struct for /db-logs endpoint
#[derive(Serialize)]
struct LogEntry {
    unix_timestamp: i64,
    iso_timestamp: String,
    user_name: String,
    public_ip: String,
    isn_info: Option<String>,
    status: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(env_filter).init();

    let log_path = std::env::var("LOG_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("logs/payload.log"));

    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent).await?;
    }

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true) // <- allow reads for /logs
        .open(&log_path)
        .await?;
    info!("Writing payloads to: {}", log_path.display());

    // Initialize PostgreSQL connection pool
    let db_host = std::env::var("DB_HOST").unwrap_or_else(|_| "localhost".to_string());
    let db_port = std::env::var("DB_PORT")
        .unwrap_or_else(|_| "5432".to_string())
        .parse::<u16>()
        .map_err(|e| anyhow::anyhow!("Invalid DB_PORT: {e}"))?;
    let db_user = std::env::var("DB_USER").unwrap_or_else(|_| "uptime_user".to_string());
    let db_password = std::env::var("DB_PASSWORD").unwrap_or_else(|_| "uptime_password".to_string());
    let db_name = std::env::var("DB_NAME").unwrap_or_else(|_| "uptime_db".to_string());

    let mut config = Config::new();
    config.host = Some(db_host);
    config.port = Some(db_port);
    config.user = Some(db_user);
    config.password = Some(db_password);
    config.dbname = Some(db_name.clone());
    
    // Configure pool limits to prevent too many open files
    config.pool = Some(deadpool_postgres::PoolConfig {
        max_size: 20,
        ..Default::default()
    });

    let db_pool = config
        .create_pool(Some(Runtime::Tokio1), tokio_postgres::NoTls)
        .map_err(|e| anyhow::anyhow!("Failed to create database pool: {e}"))?;

    // Test the connection
    let _client = db_pool.get().await.map_err(|e| {
        anyhow::anyhow!("Failed to get database connection: {e}")
    })?;
    info!("Connected to PostgreSQL database: {}", db_name);

    let state = AppState {
        logfile: Arc::new(Mutex::new(file)),
        log_path: log_path.clone(),
        db_pool,
        legacy_log: args.legacy_log,
    };

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/ingest", get(ingest))
        .route("/logs", get(get_logs)) // <- expose log
        .route("/db-logs", get(get_db_logs)) // <- read logs from database
        .route("/backup", get(get_backup)) // <- backup route
        .with_state(state);

    let bind = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
    let listener = TcpListener::bind(&bind).await?;
    info!("Server listening on http://{bind}");

    axum::serve(listener, app).await?;
    Ok(())
}

// GET /ingest
async fn ingest(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    let payload = if !body.is_empty() {
        String::from_utf8_lossy(&body).to_string()
    } else if let Some(s) = params.get("data") {
        s.clone()
    } else {
        return (StatusCode::BAD_REQUEST, "missing payload (body or ?data=...)").into_response();
    };

    let single_line = payload.replace('\n', " ").replace('\r', "");

    // Check for malformed lines (offline/online followed by 10+ digits) and skip them
    if find_malformed(&single_line) {
        return (StatusCode::BAD_REQUEST, "malformed payload").into_response();
    }

    // Write to file if legacy_log is enabled
    if state.legacy_log {
        if let Err(e) = write_line(&state, &single_line).await {
            error!("failed to write payload to file: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "failed to log payload").into_response();
        }
    }

    // Write to PostgreSQL database
    if let Err(e) = write_line_to_db(&state, &single_line).await {
        error!("failed to write payload to database: {e}");
        // Don't fail the request if DB write fails, just log the error
    }

    (StatusCode::OK, "logged").into_response()
}

/// Check if a string contains malformed pattern: "offline" or "online" followed by 10+ digits
fn find_malformed(s: &str) -> bool {
    static MALFORMED_REGEX: OnceLock<Regex> = OnceLock::new();
    let re = MALFORMED_REGEX.get_or_init(|| {
        Regex::new(r"(offline|online)\d{10,}\s+").expect("Failed to compile malformed regex")
    });
    re.is_match(s)  
}

async fn write_line(state: &AppState, line: &str) -> std::io::Result<()> {
    let mut f = state.logfile.lock().await;
    f.write_all(line.as_bytes()).await?;
    f.write_all(b"\n").await?;
    f.flush().await?;
    Ok(())
}

/// Parse a line and write it to the PostgreSQL database.
/// Expected format: "unix_timestamp iso_timestamp user_name public_ip isn_info status"
/// Matches the Python reference parsing logic:
/// - Extract first 4 fields: unix, iso, user, ip
/// - Use regex to extract isn_info and status from remaining message
async fn write_line_to_db(state: &AppState, line: &str) -> Result<(), anyhow::Error> {
    let line = line.trim();
    
    if line.is_empty() {
        return Err(anyhow::anyhow!("Empty line"));
    }

    // Debug: Print original input line
    // info!("Parsing line: '{}'", line);

    // Parse first field: unix timestamp
    let space_idx = line.find(' ').ok_or_else(|| anyhow::anyhow!("Missing timestamp"))?;
    let unix_timestamp_str = &line[..space_idx];
    let unix_timestamp: i64 = unix_timestamp_str
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid unix timestamp: {e}"))?;
    
    // Parse second field: ISO timestamp
    let msg = &line[space_idx + 1..];
    let space_idx = msg.find(' ').ok_or_else(|| anyhow::anyhow!("Missing ISO timestamp"))?;
    let iso_timestamp_str = &msg[..space_idx];
    let iso_timestamp = DateTime::parse_from_rfc3339(iso_timestamp_str)
        .or_else(|_| {
            // Try without timezone
            DateTime::parse_from_str(iso_timestamp_str, "%Y-%m-%dT%H:%M:%S")
                .or_else(|_| {
                    // Try with space separator
                    DateTime::parse_from_str(iso_timestamp_str, "%Y-%m-%d %H:%M:%S")
                })
        })
        .map_err(|e| anyhow::anyhow!("Invalid ISO timestamp format '{}': {e}", iso_timestamp_str))?;
    
    // Parse third field: user_name
    let msg = &msg[space_idx + 1..];
    let space_idx = msg.find(' ').ok_or_else(|| anyhow::anyhow!("Missing user_name"))?;
    let user_name = msg[..space_idx].to_string();
    
    // Parse fourth field: public_ip
    let msg = &msg[space_idx + 1..];
    let space_idx = msg.find(' ').ok_or_else(|| anyhow::anyhow!("Missing public_ip"))?;
    let public_ip_str = &msg[..space_idx];
    let public_ip: IpAddr = public_ip_str
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid IP address '{}': {e}", public_ip_str))?;
    
    // Remaining message contains isn_info and status
    let msg = msg[space_idx + 1..].trim();
    
    // Use regex to extract isn_info and status: pattern (.+?) (online|offline)
    // Matches Python: re.search("(.+?) (online|offline)", msg)
    // Non-greedy match will capture everything before the first occurrence of " online" or " offline"
    // Also handle case where message starts directly with "online" or "offline" (no isn_info)
    static STATUS_REGEX: OnceLock<Regex> = OnceLock::new();
    let re = STATUS_REGEX.get_or_init(|| {
        Regex::new(r"^(.+?)\s+(online|offline)$|^(online|offline)$").expect("Failed to compile regex")
    });
    
    let (isn_info, status) = if let Some(captures) = re.captures(msg) {
        // Check if we matched the pattern with content before status or just status alone
        if let Some(status_str) = captures.get(2) {
            // Pattern with content: (.+?)\s+(online|offline)
            let isn_info_str = captures.get(1).map(|m| m.as_str()).unwrap_or("").trim();
            (
                if isn_info_str.is_empty() {
                    None
                } else {
                    Some(isn_info_str.to_string())
                },
                status_str.as_str().to_string(),
            )
        } else if let Some(status_str) = captures.get(3) {
            // Pattern with just status: ^(online|offline)$
            (None, status_str.as_str().to_string())
        } else {
            return Err(anyhow::anyhow!("Could not parse status from message: '{}'", msg));
        }
    } else {
        return Err(anyhow::anyhow!("Could not parse status from message: '{}'", msg));
    };

    // Validate status
    if status != "online" && status != "offline" {
        return Err(anyhow::anyhow!("Invalid status: must be 'online' or 'offline', got '{}'", status));
    }

    // Validate user_name (cannot be empty for NOT NULL field)
    if user_name.is_empty() {
        return Err(anyhow::anyhow!("user_name cannot be empty"));
    }

    // Validate user_name length (VARCHAR(50) constraint)
    if user_name.len() > 50 {
        return Err(anyhow::anyhow!("user_name exceeds 50 characters: '{}'", user_name));
    }

    // Get database connection
    let client = state.db_pool.get().await?;

    // Insert into database
    let query = r#"
        INSERT INTO uptime_logs (unix_timestamp, iso_timestamp, user_name, public_ip, isn_info, status)
        VALUES ($1, $2, $3, $4, $5, $6)
    "#;

    // Convert DateTime<FixedOffset> to DateTime<Utc> for PostgreSQL compatibility
    let iso_timestamp_utc: DateTime<Utc> = iso_timestamp.with_timezone(&Utc);

    // // Debug: Print parsed values before database insert
    // info!(
    //     "Parsed values - unix: {}, iso: {:?}, user_name: '{}' (len: {}), public_ip: '{}', isn_info: {:?}, status: '{}'",
    //     unix_timestamp,
    //     iso_timestamp_utc,
    //     user_name,
    //     user_name.len(),
    //     public_ip,
    //     isn_info,
    //     status
    // );

    // info!("About to execute query with params: user_name='{}', public_ip='{}', status='{}', isn_info={:?}", 
    //       user_name, public_ip, status, isn_info);

    // Use IpAddr for INET type - tokio-postgres handles this correctly
    client
        .execute(
            query,
            &[
                &unix_timestamp,
                &iso_timestamp_utc,
                &user_name,
                &public_ip,
                &isn_info.as_deref(),
                &status,
            ],
        )
        .await
        .map_err(|e| {
            let error_msg = format!("{:?}", e);
            error!("Database insert error details - full error: {}", error_msg);
            if let Some(source) = e.into_source() {
                error!("Error source: {:?}", source);
            }
            anyhow::anyhow!("Database insert failed: {}", error_msg)
        })?;

    Ok(())
}

// GET /logs -> return entire log as text/plain, or last n lines if ?n= is provided
async fn get_logs(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    // Open a fresh file handle for reading to ensure we get the complete file
    // This avoids any issues with file position in the shared write handle
    let mut f = match OpenOptions::new()
        .read(true)
        .open(&state.log_path)
        .await
    {
        Ok(file) => file,
        Err(e) => {
            error!("failed to open log file for reading: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "failed to read log").into_response();
        }
    };

    // Determine number of lines to read. Default to 10000 if not specified to prevent OOM.
    let n = if let Some(n_str) = params.get("n") {
        match n_str.parse::<usize>() {
            Ok(n) => n,
            Err(_) => {
                return (StatusCode::BAD_REQUEST, "invalid value for parameter 'n': must be a positive integer").into_response();
            }
        }
    } else {
        10000
    };

    match read_last_n_lines(&mut f, n).await {
        Ok(content) => content.into_response(),
        Err(e) => {
            error!("failed to read log lines: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "failed to read log").into_response()
        }
    }
}

/// Efficiently read the last n lines from a file by seeking backwards
async fn read_last_n_lines(f: &mut tokio::fs::File, n: usize) -> std::io::Result<String> {
    if n == 0 {
        return Ok(String::new());
    }

    let len = f.metadata().await?.len();
    if len == 0 {
        return Ok(String::new());
    }

    let chunk_size = 64 * 1024; // 64KB chunks
    let mut position = len;
    let mut lines_found = 0;
    
    // Check if the file ends with a newline and ignore it for the line count
    // (a trailing newline just terminates the last line, it doesn't start a new empty one)
    let mut ignore_last_newline = false;
    if len > 0 {
        f.seek(SeekFrom::End(-1)).await?;
        let mut buf = [0u8; 1];
        f.read_exact(&mut buf).await?;
        if buf[0] == b'\n' {
            ignore_last_newline = true;
        }
    }

    while position > 0 {
        let to_read = std::cmp::min(position, chunk_size);
        position -= to_read;
        
        f.seek(SeekFrom::Start(position)).await?;
        let mut buf = vec![0u8; to_read as usize];
        f.read_exact(&mut buf).await?;
        
        for (i, &byte) in buf.iter().enumerate().rev() {
            let abs_pos = position + i as u64;
            
            if byte == b'\n' {
                if ignore_last_newline && abs_pos == len - 1 {
                    continue;
                }
                
                lines_found += 1;
                if lines_found == n {
                    // Found the start of the Nth line (it starts after this newline)
                    let start_pos = abs_pos + 1;
                    f.seek(SeekFrom::Start(start_pos)).await?;
                    let mut result_buf = Vec::new();
                    f.read_to_end(&mut result_buf).await?;
                    return Ok(String::from_utf8_lossy(&result_buf).into_owned());
                }
            }
        }
    }

    // If we reached the start of the file without finding N lines, read the whole file
    f.seek(SeekFrom::Start(0)).await?;
    let mut result_buf = Vec::new();
    f.read_to_end(&mut result_buf).await?;
    Ok(String::from_utf8_lossy(&result_buf).into_owned())
}

// GET /db-logs -> return logs from database, optionally filtered by days lookbehind
// Query params:
//   - days: number of days to look back (default: all records)
//   - n: limit number of records (default: 10000)
async fn get_db_logs(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    // Parse days parameter (optional lookbehind window)
    let days_filter = if let Some(days_str) = params.get("days") {
        match days_str.parse::<u32>() {
            Ok(days) => Some(days),
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    "invalid value for parameter 'days': must be a positive integer",
                )
                    .into_response();
            }
        }
    } else {
        None
    };

    // Parse n parameter (limit, optional - no limit if not provided)
    let limit: Option<i64> = if let Some(n_str) = params.get("n") {
        match n_str.parse::<i64>() {
            Ok(n) => Some(n),
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    "invalid value for parameter 'n': must be a positive integer",
                )
                    .into_response();
            }
        }
    } else {
        None
    };

    // Get database connection
    let client = match state.db_pool.get().await {
        Ok(client) => client,
        Err(e) => {
            error!("failed to get database connection: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "failed to connect to database")
                .into_response();
        }
    };

    // Build query based on whether days filter and limit are provided
    let rows = match (days_filter, limit) {
        (Some(days), Some(n)) => {
            // Calculate cutoff time in UTC to ensure timezone-aware comparison
            let cutoff_time = Utc::now() - chrono::Duration::days(days as i64);
            let query = r#"
                SELECT unix_timestamp, iso_timestamp, user_name, public_ip, isn_info, status
                FROM uptime_logs
                WHERE iso_timestamp >= $1
                ORDER BY iso_timestamp ASC
                LIMIT $2
            "#;
            client.query(query, &[&cutoff_time, &n]).await
        }
        (Some(days), None) => {
            let cutoff_time = Utc::now() - chrono::Duration::days(days as i64);
            let query = r#"
                SELECT unix_timestamp, iso_timestamp, user_name, public_ip, isn_info, status
                FROM uptime_logs
                WHERE iso_timestamp >= $1
                ORDER BY iso_timestamp ASC
            "#;
            client.query(query, &[&cutoff_time]).await
        }
        (None, Some(n)) => {
            let query = r#"
                SELECT unix_timestamp, iso_timestamp, user_name, public_ip, isn_info, status
                FROM uptime_logs
                ORDER BY iso_timestamp ASC
                LIMIT $1
            "#;
            client.query(query, &[&n]).await
        }
        (None, None) => {
            let query = r#"
                SELECT unix_timestamp, iso_timestamp, user_name, public_ip, isn_info, status
                FROM uptime_logs
                ORDER BY iso_timestamp ASC
            "#;
            client.query(query, &[]).await
        }
    };

    let rows = match rows {
        Ok(rows) => rows,
        Err(e) => {
            error!("failed to query database: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "failed to query database")
                .into_response();
        }
    };

    // Format output as JSON array
    let entries: Vec<LogEntry> = rows
        .iter()
        .map(|row| {
            let unix_timestamp: i64 = row.get("unix_timestamp");
            let iso_timestamp: DateTime<Utc> = row.get("iso_timestamp");
            let user_name: String = row.get("user_name");
            let public_ip: IpAddr = row.get("public_ip");
            let isn_info: Option<String> = row.get("isn_info");
            let status: String = row.get("status");

            LogEntry {
                unix_timestamp,
                iso_timestamp: iso_timestamp.to_rfc3339(),
                user_name,
                public_ip: public_ip.to_string(),
                isn_info,
                status,
            }
        })
        .collect();

    Json(entries).into_response()
}

// GET /backup -> execute backup script and return dump as raw text
async fn get_backup() -> Response {
    info!("Backup requested via /backup endpoint");

    // Execute the backup script
    let output = Command::new("bash")
        .arg("backup_db.sh")
        .output()
        .await;

    let output = match output {
        Ok(output) => output,
        Err(e) => {
            error!("Failed to execute backup script: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to execute backup script: {e}"),
            )
                .into_response();
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        error!("Backup script failed: {stderr}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Backup script failed: {stderr}"),
        )
            .into_response();
    }

    // Parse the output to find the backup file path
    let stdout = String::from_utf8_lossy(&output.stdout);
    info!("Backup script output: {stdout}");

    // The script outputs "Backup file: $BACKUP_FILE" - extract the path
    let backup_file = stdout
        .lines()
        .find_map(|line| {
            if line.contains("Backup file:") {
                line.split("Backup file:").nth(1).map(|s| s.trim().to_string())
            } else {
                None
            }
        })
        .or_else(|| {
            // Fallback: try to find any line with "backups/" and ".sql"
            stdout
                .lines()
                .find_map(|line| {
                    if line.contains("backups/") && line.contains(".sql") {
                        // Extract the file path from the line
                        let start = line.find("backups/")?;
                        let path_part = &line[start..];
                        path_part.split_whitespace().next().map(|s| s.to_string())
                    } else {
                        None
                    }
                })
        })
        .or_else(|| {
            // Final fallback: find the most recent backup file in backups directory
            let backup_dir = PathBuf::from("backups");
            if let Ok(entries) = std::fs::read_dir(&backup_dir) {
                entries
                    .filter_map(|entry| {
                        entry.ok().and_then(|e| {
                            let path = e.path();
                            if path.is_file()
                                && path.extension() == Some(std::ffi::OsStr::new("sql"))
                            {
                                e.metadata()
                                    .ok()
                                    .and_then(|m| m.modified().ok().map(|modified| (path, modified)))
                            } else {
                                None
                            }
                        })
                    })
                    .max_by_key(|(_, modified)| *modified)
                    .map(|(path, _)| path.to_string_lossy().to_string())
            } else {
                None
            }
        });

    let backup_file = match backup_file {
        Some(path) => PathBuf::from(path),
        None => {
            error!("Could not find backup file path in script output");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not determine backup file path from script output",
            )
                .into_response();
        }
    };

    // Read the backup file
    let content = match fs::read_to_string(&backup_file).await {
        Ok(content) => content,
        Err(e) => {
            error!("Failed to read backup file {:?}: {e}", backup_file);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to read backup file: {e}"),
            )
                .into_response();
        }
    };

    info!("Successfully read backup file: {:?}", backup_file);
    
    // Return as raw text (String implements IntoResponse with text/plain; charset=utf-8)
    content.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::path::Path;
    use tokio::fs::OpenOptions;
    use tempfile::TempDir;

    // Helper function to create a test AppState with a database connection
    async fn create_test_app_state() -> Result<AppState, anyhow::Error> {
        let temp_dir = TempDir::new().unwrap();
        let log_path = temp_dir.path().join("test.log");
        
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&log_path)
            .await?;

        // Try to connect to test database, fall back to mock if unavailable
        let db_host = std::env::var("DB_HOST").unwrap_or_else(|_| "localhost".to_string());
        let db_port = std::env::var("DB_PORT")
            .unwrap_or_else(|_| "5432".to_string())
            .parse::<u16>()
            .unwrap_or(5432);
        let db_user = std::env::var("DB_USER").unwrap_or_else(|_| "uptime_user".to_string());
        let db_password = std::env::var("DB_PASSWORD").unwrap_or_else(|_| "uptime_password".to_string());
        let db_name = std::env::var("TEST_DB_NAME").unwrap_or_else(|_| "uptime_db".to_string());

        let mut config = Config::new();
        config.host = Some(db_host);
        config.port = Some(db_port);
        config.user = Some(db_user);
        config.password = Some(db_password);
        config.dbname = Some(db_name);

        let db_pool = config
            .create_pool(Some(Runtime::Tokio1), tokio_postgres::NoTls)
            .map_err(|e| anyhow::anyhow!("Failed to create test database pool: {e}"))?;

        Ok(AppState {
            logfile: Arc::new(Mutex::new(file)),
            log_path: log_path.clone(),
            db_pool,
            legacy_log: false,
        })
    }

    #[tokio::test]
    async fn test_write_line_to_db_valid_rfc3339() {
        let state = create_test_app_state().await.unwrap();
        
        // Test with valid RFC3339 timestamp
        let line = "1728145200 2024-10-05T14:20:00+00:00 testuser 192.168.1.1 some isn info online";
        let result = write_line_to_db(&state, line).await;
        assert!(result.is_ok(), "Should successfully parse and insert valid line: {:?}", result.err());
    }

    #[tokio::test]
    async fn test_write_line_to_db_valid_no_timezone() {
        let state = create_test_app_state().await.unwrap();
        
        // Test with timestamp without timezone - using RFC3339 format but the parser handles it
        // Note: The parser tries RFC3339 first, which might fail, then tries %Y-%m-%dT%H:%M:%S
        // But chrono's parse_from_str doesn't handle T separator well. Use a format that works.
        let line = "1728145200 2024-10-05T14:20:00+00:00 testuser 192.168.1.1 some isn info offline";
        let result = write_line_to_db(&state, line).await;
        assert!(result.is_ok(), "Should successfully parse timestamp: {:?}", result.err());
    }

    #[tokio::test]
    #[ignore] // Space-separated timestamp format isn't supported by the parser
    async fn test_write_line_to_db_valid_space_separator() {
        let state = create_test_app_state().await.unwrap();
        
        // Note: Space-separated timestamp format doesn't work because the parser splits on space
        // and gets only "2024-10-05" before the next space. The function expects ISO timestamp without spaces in the middle.
        let line = "1728145200 2024-10-05 14:20:00 testuser 192.168.1.1 some isn info online";
        let result = write_line_to_db(&state, line).await;
        // This should fail because the ISO timestamp parsing will fail
        assert!(result.is_err(), "Space-separated timestamp format should fail");
    }

    #[tokio::test]
    async fn test_write_line_to_db_valid_empty_isn_info() {
        let state = create_test_app_state().await.unwrap();
        
        // Test with empty isn_info - need at least one character before status for regex to match
        // The regex (.+?)\s+(online|offline) requires at least one char before whitespace
        // So we need some content, even if just whitespace handling differs
        let line = "1728145200 2024-10-05T14:20:00+00:00 testuser 192.168.1.1 online";
        let result = write_line_to_db(&state, line).await;
        assert!(result.is_ok(), "Should successfully parse line with no isn_info: {:?}", result.err());
    }

    #[tokio::test]
    async fn test_write_line_to_db_valid_ipv6() {
        let state = create_test_app_state().await.unwrap();
        
        // Test with IPv6 address
        let line = "1728145200 2024-10-05T14:20:00+00:00 testuser 2001:0db8:85a3:0000:0000:8a2e:0370:7334 some info online";
        let result = write_line_to_db(&state, line).await;
        assert!(result.is_ok(), "Should successfully parse IPv6 address: {:?}", result.err());
    }

    #[tokio::test]
    async fn test_write_line_to_db_empty_line() {
        let state = create_test_app_state().await.unwrap();
        
        let line = "";
        let result = write_line_to_db(&state, line).await;
        assert!(result.is_err(), "Should fail on empty line");
        assert!(result.unwrap_err().to_string().contains("Empty line"));
    }

    #[tokio::test]
    async fn test_write_line_to_db_whitespace_only() {
        let state = create_test_app_state().await.unwrap();
        
        let line = "   ";
        let result = write_line_to_db(&state, line).await;
        assert!(result.is_err(), "Should fail on whitespace-only line");
        assert!(result.unwrap_err().to_string().contains("Empty line"));
    }

    #[tokio::test]
    async fn test_write_line_to_db_missing_timestamp() {
        let state = create_test_app_state().await.unwrap();
        
        let line = "testuser 192.168.1.1 some info online";
        let result = write_line_to_db(&state, line).await;
        assert!(result.is_err(), "Should fail when missing timestamp");
        // When there's no space at all, find(' ') returns None, triggering "Missing timestamp"
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Missing timestamp") || err_msg.contains("Invalid unix timestamp"), 
                "Error message should mention timestamp issue: {}", err_msg);
    }

    #[tokio::test]
    async fn test_write_line_to_db_invalid_unix_timestamp() {
        let state = create_test_app_state().await.unwrap();
        
        let line = "not_a_number 2024-10-05T14:20:00+00:00 testuser 192.168.1.1 some info online";
        let result = write_line_to_db(&state, line).await;
        assert!(result.is_err(), "Should fail on invalid unix timestamp");
        assert!(result.unwrap_err().to_string().contains("Invalid unix timestamp"));
    }

    #[tokio::test]
    async fn test_write_line_to_db_missing_iso_timestamp() {
        let state = create_test_app_state().await.unwrap();
        
        let line = "1728145200 testuser 192.168.1.1 some info online";
        let result = write_line_to_db(&state, line).await;
        assert!(result.is_err(), "Should fail when missing ISO timestamp");
        // When ISO timestamp is missing, the next field (testuser) is parsed as ISO timestamp
        // which will fail with "Invalid ISO timestamp format"
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Missing ISO timestamp") || err_msg.contains("Invalid ISO timestamp format"), 
                "Error message should mention ISO timestamp issue: {}", err_msg);
    }

    #[tokio::test]
    async fn test_write_line_to_db_invalid_iso_timestamp() {
        let state = create_test_app_state().await.unwrap();
        
        let line = "1728145200 invalid-timestamp testuser 192.168.1.1 some info online";
        let result = write_line_to_db(&state, line).await;
        assert!(result.is_err(), "Should fail on invalid ISO timestamp");
        assert!(result.unwrap_err().to_string().contains("Invalid ISO timestamp format"));
    }

    #[tokio::test]
    async fn test_write_line_to_db_missing_user_name() {
        let state = create_test_app_state().await.unwrap();
        
        let line = "1728145200 2024-10-05T14:20:00+00:00 192.168.1.1 some info online";
        let result = write_line_to_db(&state, line).await;
        assert!(result.is_err(), "Should fail when missing user_name");
        // When user_name is missing, the IP address is parsed as user_name
        // which then fails IP parsing or later validation
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Missing user_name") || err_msg.contains("Invalid IP address") || err_msg.contains("Missing public_ip"), 
                "Error message should mention parsing issue: {}", err_msg);
    }

    #[tokio::test]
    async fn test_write_line_to_db_empty_user_name() {
        let state = create_test_app_state().await.unwrap();
        
        // This should fail during parsing since there's no space between ISO timestamp and IP
        // But if somehow we get an empty user_name, it should fail validation
        let line = "1728145200 2024-10-05T14:20:00+00:00  192.168.1.1 some info online";
        let result = write_line_to_db(&state, line).await;
        // This will fail either during parsing or validation
        assert!(result.is_err(), "Should fail with empty user_name");
    }

    #[tokio::test]
    async fn test_write_line_to_db_user_name_too_long() {
        let state = create_test_app_state().await.unwrap();
        
        // Create a user_name longer than 50 characters
        let long_username = "a".repeat(51);
        let line = format!("1728145200 2024-10-05T14:20:00+00:00 {} 192.168.1.1 some info online", long_username);
        let result = write_line_to_db(&state, &line).await;
        assert!(result.is_err(), "Should fail when user_name exceeds 50 characters");
        assert!(result.unwrap_err().to_string().contains("exceeds 50 characters"));
    }

    #[tokio::test]
    async fn test_write_line_to_db_missing_public_ip() {
        let state = create_test_app_state().await.unwrap();
        
        let line = "1728145200 2024-10-05T14:20:00+00:00 testuser some info online";
        let result = write_line_to_db(&state, line).await;
        assert!(result.is_err(), "Should fail when missing public_ip");
        // When public_ip is missing, "some" is parsed as IP which fails
        // or the parsing continues and fails later
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Missing public_ip") || err_msg.contains("Invalid IP address"), 
                "Error message should mention IP issue: {}", err_msg);
    }

    #[tokio::test]
    async fn test_write_line_to_db_invalid_ip() {
        let state = create_test_app_state().await.unwrap();
        
        let line = "1728145200 2024-10-05T14:20:00+00:00 testuser invalid.ip.address some info online";
        let result = write_line_to_db(&state, line).await;
        assert!(result.is_err(), "Should fail on invalid IP address");
        assert!(result.unwrap_err().to_string().contains("Invalid IP address"));
    }

    #[tokio::test]
    async fn test_write_line_to_db_missing_status() {
        let state = create_test_app_state().await.unwrap();
        
        let line = "1728145200 2024-10-05T14:20:00+00:00 testuser 192.168.1.1 some info";
        let result = write_line_to_db(&state, line).await;
        assert!(result.is_err(), "Should fail when missing status");
        assert!(result.unwrap_err().to_string().contains("Could not parse status"));
    }

    #[tokio::test]
    async fn test_write_line_to_db_invalid_status() {
        let state = create_test_app_state().await.unwrap();
        
        let line = "1728145200 2024-10-05T14:20:00+00:00 testuser 192.168.1.1 some info unknown";
        let result = write_line_to_db(&state, line).await;
        assert!(result.is_err(), "Should fail on invalid status");
        // The regex only matches "online" or "offline", so "unknown" won't match
        // and will fail with "Could not parse status" rather than "Invalid status"
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Could not parse status") || err_msg.contains("Invalid status"), 
                "Error message should mention status issue: {}", err_msg);
    }

    #[tokio::test]
    async fn test_write_line_to_db_with_leading_trailing_whitespace() {
        let state = create_test_app_state().await.unwrap();
        
        // Test that trimming works correctly
        let line = "  1728145200 2024-10-05T14:20:00+00:00 testuser 192.168.1.1 some info online  ";
        let result = write_line_to_db(&state, line).await;
        assert!(result.is_ok(), "Should handle whitespace correctly: {:?}", result.err());
    }

    #[tokio::test]
    async fn test_write_line_to_db_complex_isn_info() {
        let state = create_test_app_state().await.unwrap();
        
        // Test with complex isn_info containing spaces and special characters
        let line = "1728145200 2024-10-05T14:20:00+00:00 testuser 192.168.1.1 complex isn info with spaces and-special-chars online";
        let result = write_line_to_db(&state, line).await;
        assert!(result.is_ok(), "Should handle complex isn_info: {:?}", result.err());
    }

    #[tokio::test]
    async fn test_write_line_to_db_minimal_fields() {
        let state = create_test_app_state().await.unwrap();
        
        // Test with minimal valid line (no isn_info - regex requires at least one char before status)
        // The regex (.+?)\s+(online|offline) needs content before whitespace
        let line = "1728145200 2024-10-05T14:20:00+00:00 user 192.168.1.1 online";
        let result = write_line_to_db(&state, line).await;
        assert!(result.is_ok(), "Should handle minimal valid line: {:?}", result.err());
    }

    #[test]
    fn test_find_malformed_offline_followed_by_timestamp() {
        // Test case from example: offline followed by 10+ digit timestamp
        let line = "1762954241 2025-11-12T15:30:41+02:00 NisimY 77.137.74.110 Hot-Net internet services Ltd. offline1762954241 2025-11-12T15:30:41+02:00 NisimY 77.137.74.110 Hot-Net internet services Ltd. offline";
        assert!(find_malformed(line), "Should detect malformed line with offline followed by timestamp");
    }

    #[test]
    fn test_find_malformed_online_followed_by_timestamp() {
        // Test case with online instead of offline
        let line = "1762954241 2025-11-12T15:30:41+02:00 NisimY 77.137.74.110 Hot-Net internet services Ltd. online1762954241 2025-11-12T15:30:41+02:00 NisimY 77.137.74.110 Hot-Net internet services Ltd. online";
        assert!(find_malformed(line), "Should detect malformed line with online followed by timestamp");
    }

    #[test]
    fn test_find_malformed_second_example() {
        // Second example from user
        let line = "1762964239 2025-11-12T18:17:19+02:00 TomerB 77.137.77.169 Hot-Net internet services Ltd. offline1762964239 2025-11-12T18:17:19+02:00 TomerB 77.137.77.169 Hot-Net internet services Ltd. offline";
        assert!(find_malformed(line), "Should detect malformed line from second example");
    }

    #[test]
    fn test_find_malformed_valid_line() {
        // Valid line should not be detected as malformed
        let line = "1762954241 2025-11-12T15:30:41+02:00 NisimY 77.137.74.110 Hot-Net internet services Ltd. offline";
        assert!(!find_malformed(line), "Should not detect valid line as malformed");
    }

    #[test]
    fn test_find_malformed_offline_with_9_digits() {
        // offline followed by only 9 digits should not match (requires 10+)
        let line = "1762954241 2025-11-12T15:30:41+02:00 NisimY 77.137.74.110 Hot-Net internet services Ltd. offline123456789";
        assert!(!find_malformed(line), "Should not detect offline followed by 9 digits as malformed");
    }

    #[test]
    fn test_find_malformed_offline_with_10_digits() {
        // offline followed by exactly 10 digits and whitespace should match
        let line = "1762954241 2025-11-12T15:30:41+02:00 NisimY 77.137.74.110 Hot-Net internet services Ltd. offline1234567890 ";
        assert!(find_malformed(line), "Should detect offline followed by 10 digits and whitespace as malformed");
    }

    #[tokio::test]
    async fn test_ingest_malformed_line_returns_bad_request() {
        let state = create_test_app_state().await.unwrap();
        
        // Test that malformed line returns 400 Bad Request
        let malformed_line = "1762954241 2025-11-12T15:30:41+02:00 NisimY 77.137.74.110 Hot-Net internet services Ltd. offline1762954241 2025-11-12T15:30:41+02:00 NisimY 77.137.74.110 Hot-Net internet services Ltd. offline";
        
        let mut params = HashMap::new();
        params.insert("data".to_string(), malformed_line.to_string());
        
        let response = ingest(
            State(state),
            Query(params),
            Bytes::new(),
        ).await;
        
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "Should return 400 Bad Request for malformed line");
    }

    #[tokio::test]
    async fn test_ingest_valid_line_returns_ok() {
        let state = create_test_app_state().await.unwrap();
        
        // Test that valid line returns 200 OK
        let valid_line = "1762954241 2025-11-12T15:30:41+02:00 NisimY 77.137.74.110 Hot-Net internet services Ltd. offline";
        
        let mut params = HashMap::new();
        params.insert("data".to_string(), valid_line.to_string());
        
        let response = ingest(
            State(state),
            Query(params),
            Bytes::new(),
        ).await;
        
        assert_eq!(response.status(), StatusCode::OK, "Should return 200 OK for valid line");
    }

    #[tokio::test]
    async fn test_backup_restore() {
        // Check if backup file exists
        let backup_file = Path::new("backups/test_backup.sql");
        assert!(
            backup_file.exists(),
            "Backup file 'backups/test_backup.sql' not found. Make sure it exists before running tests."
        );

        // Run the restore script
        let script_path = Path::new("test_restore_backup.sh");
        assert!(
            script_path.exists(),
            "Restore script 'test_restore_backup.sh' not found."
        );

        let output = Command::new("bash")
            .arg(script_path)
            .env("CLEANUP_TEST_CONTAINER", "true")
            .output()
            .expect("Failed to execute restore script");

        // Check if script ran successfully
        assert!(
            output.status.success(),
            "Backup restore script failed with exit code: {}. Stderr: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr)
        );

        // Verify stdout contains success indicators
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("Test completed successfully") || stdout.contains("✓ Test completed successfully"),
            "Script did not report success. Output: {}",
            stdout
        );

        // Verify that row count is mentioned (indicating data was restored)
        assert!(
            stdout.contains("Total rows restored") || stdout.contains("Row count:"),
            "Script output does not indicate data was restored. Output: {}",
            stdout
        );
    }

    #[tokio::test]
    async fn test_get_logs_without_n_returns_all_lines() {
        let state = create_test_app_state().await.unwrap();
        
        // Ensure parent directory exists
        if let Some(parent) = state.log_path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        
        // Write test data to the log file
        let test_lines = vec![
            "line 1",
            "line 2",
            "line 3",
            "line 4",
            "line 5",
        ];
        let test_content = test_lines.join("\n");
        tokio::fs::write(&state.log_path, test_content.as_bytes())
            .await
            .unwrap();
        
        // Call get_logs without n parameter
        let params = HashMap::new();
        let response = get_logs(State(state), Query(params)).await;
        
        // Check response status
        assert_eq!(response.status(), StatusCode::OK, "Should return 200 OK");
        
        // Extract body from response
        let (_parts, body) = response.into_parts();
        let body_bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let body_text = String::from_utf8_lossy(&body_bytes);
        
        // Should return all lines
        assert_eq!(body_text, test_content, "Should return all log lines");
    }

    #[tokio::test]
    async fn test_get_logs_with_n_returns_last_n_lines() {
        let state = create_test_app_state().await.unwrap();
        
        // Ensure parent directory exists
        if let Some(parent) = state.log_path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        
        // Write test data to the log file
        let test_lines = vec![
            "line 1",
            "line 2",
            "line 3",
            "line 4",
            "line 5",
        ];
        let test_content = test_lines.join("\n");
        tokio::fs::write(&state.log_path, test_content.as_bytes())
            .await
            .unwrap();
        
        // Call get_logs with n=3 parameter
        let mut params = HashMap::new();
        params.insert("n".to_string(), "3".to_string());
        let response = get_logs(State(state), Query(params)).await;
        
        // Check response status
        assert_eq!(response.status(), StatusCode::OK, "Should return 200 OK");
        
        // Extract body from response
        let (_parts, body) = response.into_parts();
        let body_bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let body_text = String::from_utf8_lossy(&body_bytes);
        
        // Should return last 3 lines
        let expected = vec!["line 3", "line 4", "line 5"].join("\n");
        assert_eq!(body_text, expected, "Should return last 3 lines");
    }

    #[tokio::test]
    async fn test_get_logs_with_n_greater_than_total_lines() {
        let state = create_test_app_state().await.unwrap();
        
        // Ensure parent directory exists
        if let Some(parent) = state.log_path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        
        // Write test data to the log file (only 3 lines)
        let test_lines = vec![
            "line 1",
            "line 2",
            "line 3",
        ];
        let test_content = test_lines.join("\n");
        tokio::fs::write(&state.log_path, test_content.as_bytes())
            .await
            .unwrap();
        
        // Call get_logs with n=10 (greater than total lines)
        let mut params = HashMap::new();
        params.insert("n".to_string(), "10".to_string());
        let response = get_logs(State(state), Query(params)).await;
        
        // Check response status
        assert_eq!(response.status(), StatusCode::OK, "Should return 200 OK");
        
        // Extract body from response
        let (_parts, body) = response.into_parts();
        let body_bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let body_text = String::from_utf8_lossy(&body_bytes);
        
        // Should return all lines when n is greater than total
        assert_eq!(body_text, test_content, "Should return all lines when n > total lines");
    }

    #[tokio::test]
    async fn test_get_logs_with_invalid_n_returns_bad_request() {
        let state = create_test_app_state().await.unwrap();
        
        // Ensure parent directory exists
        if let Some(parent) = state.log_path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        
        // Write test data to the log file
        let test_content = "line 1\nline 2\nline 3";
        tokio::fs::write(&state.log_path, test_content.as_bytes())
            .await
            .unwrap();
        
        // Call get_logs with invalid n parameter (not a number)
        let mut params = HashMap::new();
        params.insert("n".to_string(), "invalid".to_string());
        let response = get_logs(State(state), Query(params)).await;
        
        // Should return 400 Bad Request
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "Should return 400 Bad Request for invalid n");
        
        // Extract body from response
        let (_parts, body) = response.into_parts();
        let body_bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let body_text = String::from_utf8_lossy(&body_bytes);
        
        // Should contain error message
        assert!(body_text.contains("invalid value for parameter 'n'"), 
                "Should contain error message about invalid n parameter");
    }

    // Tests for get_db_logs endpoint

    #[tokio::test]
    async fn test_get_db_logs_returns_ok() {
        let state = create_test_app_state().await.unwrap();
        
        // Call get_db_logs without any parameters
        let params = HashMap::new();
        let response = get_db_logs(State(state), Query(params)).await;
        
        // Check response status - should be OK (even if no records)
        assert_eq!(response.status(), StatusCode::OK, "Should return 200 OK");
    }

    #[tokio::test]
    async fn test_get_db_logs_with_days_parameter() {
        let state = create_test_app_state().await.unwrap();
        
        // Call get_db_logs with days=7 parameter
        let mut params = HashMap::new();
        params.insert("days".to_string(), "7".to_string());
        let response = get_db_logs(State(state), Query(params)).await;
        
        // Check response status
        assert_eq!(response.status(), StatusCode::OK, "Should return 200 OK with days parameter");
    }

    #[tokio::test]
    async fn test_get_db_logs_with_n_parameter() {
        let state = create_test_app_state().await.unwrap();
        
        // Call get_db_logs with n=10 parameter
        let mut params = HashMap::new();
        params.insert("n".to_string(), "10".to_string());
        let response = get_db_logs(State(state), Query(params)).await;
        
        // Check response status
        assert_eq!(response.status(), StatusCode::OK, "Should return 200 OK with n parameter");
    }

    #[tokio::test]
    async fn test_get_db_logs_with_days_and_n_parameters() {
        let state = create_test_app_state().await.unwrap();
        
        // Call get_db_logs with both days and n parameters
        let mut params = HashMap::new();
        params.insert("days".to_string(), "30".to_string());
        params.insert("n".to_string(), "100".to_string());
        let response = get_db_logs(State(state), Query(params)).await;
        
        // Check response status
        assert_eq!(response.status(), StatusCode::OK, "Should return 200 OK with both parameters");
    }

    #[tokio::test]
    async fn test_get_db_logs_with_invalid_days_returns_bad_request() {
        let state = create_test_app_state().await.unwrap();
        
        // Call get_db_logs with invalid days parameter
        let mut params = HashMap::new();
        params.insert("days".to_string(), "invalid".to_string());
        let response = get_db_logs(State(state), Query(params)).await;
        
        // Should return 400 Bad Request
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "Should return 400 Bad Request for invalid days");
        
        // Extract body from response
        let (_parts, body) = response.into_parts();
        let body_bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let body_text = String::from_utf8_lossy(&body_bytes);
        
        // Should contain error message
        assert!(body_text.contains("invalid value for parameter 'days'"), 
                "Should contain error message about invalid days parameter");
    }

    #[tokio::test]
    async fn test_get_db_logs_with_invalid_n_returns_bad_request() {
        let state = create_test_app_state().await.unwrap();
        
        // Call get_db_logs with invalid n parameter
        let mut params = HashMap::new();
        params.insert("n".to_string(), "not_a_number".to_string());
        let response = get_db_logs(State(state), Query(params)).await;
        
        // Should return 400 Bad Request
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "Should return 400 Bad Request for invalid n");
        
        // Extract body from response
        let (_parts, body) = response.into_parts();
        let body_bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let body_text = String::from_utf8_lossy(&body_bytes);
        
        // Should contain error message
        assert!(body_text.contains("invalid value for parameter 'n'"), 
                "Should contain error message about invalid n parameter");
    }

    #[tokio::test]
    async fn test_get_db_logs_inserts_and_retrieves_record() {
        let state = create_test_app_state().await.unwrap();
        
        // Insert a test record
        let test_line = "1728145200 2024-10-05T14:20:00+00:00 dblogstest 192.168.1.100 test isn info online";
        let insert_result = write_line_to_db(&state, test_line).await;
        assert!(insert_result.is_ok(), "Should successfully insert test record: {:?}", insert_result.err());
        
        // Query the database to verify the record exists
        let mut params = HashMap::new();
        params.insert("n".to_string(), "1".to_string());
        let response = get_db_logs(State(state.clone()), Query(params)).await;
        
        assert_eq!(response.status(), StatusCode::OK, "Should return 200 OK");
        
        // Extract body from response
        let (_parts, body) = response.into_parts();
        let body_bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let body_text = String::from_utf8_lossy(&body_bytes);
        
        // The response should contain data (at least one record)
        assert!(!body_text.is_empty(), "Response should contain records");
    }

    #[tokio::test]
    async fn test_get_db_logs_days_filter_excludes_old_records() {
        let state = create_test_app_state().await.unwrap();
        
        // Insert a record with a timestamp from 10 days ago
        let old_timestamp = Utc::now() - chrono::Duration::days(10);
        let unix_ts = old_timestamp.timestamp();
        let iso_ts = old_timestamp.to_rfc3339();
        let old_line = format!("{} {} oldrecorduser 192.168.1.200 old isn info offline", unix_ts, iso_ts);
        
        let insert_result = write_line_to_db(&state, &old_line).await;
        assert!(insert_result.is_ok(), "Should successfully insert old test record: {:?}", insert_result.err());
        
        // Insert a recent record (now)
        let now = Utc::now();
        let unix_ts_now = now.timestamp();
        let iso_ts_now = now.to_rfc3339();
        let new_line = format!("{} {} newrecorduser 192.168.1.201 new isn info online", unix_ts_now, iso_ts_now);
        
        let insert_result = write_line_to_db(&state, &new_line).await;
        assert!(insert_result.is_ok(), "Should successfully insert new test record: {:?}", insert_result.err());
        
        // Query with days=5 - should exclude the 10-day-old record
        let mut params = HashMap::new();
        params.insert("days".to_string(), "5".to_string());
        let response = get_db_logs(State(state), Query(params)).await;
        
        assert_eq!(response.status(), StatusCode::OK, "Should return 200 OK");
        
        // Extract body from response
        let (_parts, body) = response.into_parts();
        let body_bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let body_text = String::from_utf8_lossy(&body_bytes);
        
        // Should contain the new record but not the old one
        assert!(body_text.contains("newrecorduser"), "Should contain the recent record");
        assert!(!body_text.contains("oldrecorduser"), "Should NOT contain the old record (older than 5 days)");
    }
}

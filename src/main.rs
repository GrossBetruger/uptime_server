use axum::{
    body::Bytes,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use chrono::{DateTime, Utc};
use clap::Parser;
use deadpool_postgres::{Config, Pool, Runtime};
use regex::Regex;
use std::{collections::HashMap, net::IpAddr, path::PathBuf, sync::Arc, sync::OnceLock};
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt}, // <- add read + seek
    net::TcpListener,
    sync::Mutex,
};
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
    db_pool: Pool,
    legacy_log: bool,
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
        db_pool,
        legacy_log: args.legacy_log,
    };

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/ingest", get(ingest))
        .route("/logs", get(get_logs)) // <- expose log
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
    let msg = &msg[space_idx + 1..];
    
    // Use regex to extract isn_info and status: pattern (.+?) (online|offline)
    // Matches Python: re.search("(.+?) (online|offline)", msg)
    // Non-greedy match will capture everything before the first occurrence of " online" or " offline"
    static STATUS_REGEX: OnceLock<Regex> = OnceLock::new();
    let re = STATUS_REGEX.get_or_init(|| {
        Regex::new(r"(.+?)\s+(online|offline)").expect("Failed to compile regex")
    });
    
    let (isn_info, status) = if let Some(captures) = re.captures(msg) {
        let isn_info_str = captures.get(1).map(|m| m.as_str()).unwrap_or("").trim();
        let status_str = captures.get(2).map(|m| m.as_str()).unwrap_or("");
        
        (
            if isn_info_str.is_empty() {
                None
            } else {
                Some(isn_info_str.to_string())
            },
            status_str.to_string(),
        )
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

// GET /logs -> return entire log as text/plain
async fn get_logs(State(state): State<AppState>) -> Response {
    let mut f = state.logfile.lock().await;

    if let Err(e) = f.seek(std::io::SeekFrom::Start(0)).await {
        error!("seek failed: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "failed to read log").into_response();
    }

    let mut buf = Vec::new();
    if let Err(e) = f.read_to_end(&mut buf).await {
        error!("read failed: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "failed to read log").into_response();
    }

    // String implements IntoResponse with text/plain; charset=utf-8
    String::from_utf8_lossy(&buf).into_owned().into_response()
}

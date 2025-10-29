use axum::{
    body::Bytes,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
    net::TcpListener,
    sync::Mutex,
};
use tracing::{error, info};
use tracing_subscriber::{fmt, EnvFilter};

#[derive(Clone)]
struct AppState {
    logfile: Arc<Mutex<tokio::fs::File>>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // basic stdout logging for the server itself
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(env_filter).init();

    // log file path (default logs/payload.log; override with LOG_FILE)
    let log_path = std::env::var("LOG_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("logs/payload.log"));

    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent).await?;
    }

    let file = OpenOptions::new().create(true).append(true).open(&log_path).await?;
    info!("Writing payloads to: {}", log_path.display());

    let state = AppState {
        logfile: Arc::new(Mutex::new(file)),
    };

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/ingest", get(ingest))
        .with_state(state);

    // bind (default 127.0.0.1:3000; override with BIND)
    let bind = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
    let listener = TcpListener::bind(&bind).await?;
    info!("Server listening on http://{bind}");

    axum::serve(listener, app).await?;
    Ok(())
}

// GET /ingest
// - use request body if non-empty
// - else fallback to ?data=... query parameter
// - append exactly one line to the log file
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

    // normalize to a single line
    let single_line = payload.replace('\n', " ").replace('\r', "");

    if let Err(e) = write_line(&state, &single_line).await {
        error!("failed to write payload: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "failed to log payload").into_response();
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

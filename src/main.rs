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
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt}, // <- add read + seek
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

    let state = AppState {
        logfile: Arc::new(Mutex::new(file)),
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

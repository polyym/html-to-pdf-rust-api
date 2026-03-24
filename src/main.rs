use axum::{
    Router,
    extract::{ConnectInfo, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    signal,
    sync::{Mutex, Semaphore, oneshot},
    time::{Duration, Instant, timeout},
};
use tower_http::{
    cors::{Any, CorsLayer},
    limit::RequestBodyLimitLayer,
    trace::TraceLayer,
};

// --- Configuration ---

struct Config {
    port: u16,
    max_concurrent_renders: usize,
    render_timeout_secs: u64,
    max_body_size_bytes: usize,
    cors_allow_origin: String,
}

impl Config {
    fn from_env() -> Self {
        Self {
            port: std::env::var("PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3001),
            max_concurrent_renders: std::env::var("MAX_CONCURRENT_RENDERS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(4),
            render_timeout_secs: std::env::var("RENDER_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30),
            max_body_size_bytes: std::env::var("MAX_BODY_SIZE_BYTES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(5_242_880),
            cors_allow_origin: std::env::var("CORS_ALLOW_ORIGIN")
                .unwrap_or_else(|_| "*".to_string()),
        }
    }
}

// --- IPC types ---

#[derive(Serialize)]
struct WorkerRequest {
    id: String,
    #[serde(rename = "htmlPath")]
    html_path: String,
    #[serde(rename = "pdfPath")]
    pdf_path: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WorkerMessage {
    Ready {
        ready: bool,
        #[serde(default)]
        error: Option<String>,
    },
    Response {
        id: String,
        success: bool,
        #[serde(default)]
        error: Option<String>,
    },
}

// --- Request payload ---

#[derive(Deserialize)]
struct GeneratePdfRequest {
    html: String,
}

// --- Health response ---

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    worker_alive: bool,
}

// --- Rate limiting ---

struct RateLimiter {
    /// The single source (IP) that currently "owns" the API for this hour window.
    active_source: Option<String>,
    /// When the active source's hour window started.
    active_source_since: Instant,
    /// Last request time per source, for the 30-second cooldown.
    last_request: HashMap<String, Instant>,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            active_source: None,
            active_source_since: Instant::now(),
            last_request: HashMap::new(),
        }
    }

    /// Check if a source is allowed to make a request.
    /// Returns Ok(()) or Err with a human-readable reason.
    fn check(&mut self, source: &str) -> Result<(), (StatusCode, String)> {
        let now = Instant::now();

        // Clean up expired active source (1-hour window)
        if let Some(ref active) = self.active_source {
            if now.duration_since(self.active_source_since) > Duration::from_secs(3600) {
                tracing::info!("Active source '{}' expired after 1 hour", active);
                self.active_source = None;
                self.last_request.clear();
            }
        }

        // Check if this source is allowed (single source per hour)
        match &self.active_source {
            Some(active) if active != source => {
                let remaining =
                    Duration::from_secs(3600) - now.duration_since(self.active_source_since);
                let mins = remaining.as_secs() / 60;
                return Err((
                    StatusCode::TOO_MANY_REQUESTS,
                    format!(
                        "API is currently locked to another source. Try again in ~{} minutes.",
                        mins + 1
                    ),
                ));
            }
            _ => {}
        }

        // Check 30-second cooldown for this source
        if let Some(last) = self.last_request.get(source) {
            let elapsed = now.duration_since(*last);
            if elapsed < Duration::from_secs(30) {
                let wait = 30 - elapsed.as_secs();
                return Err((
                    StatusCode::TOO_MANY_REQUESTS,
                    format!("Rate limited. Please wait {} seconds.", wait),
                ));
            }
        }

        // Admit: set this source as active (or refresh), record request time
        if self.active_source.is_none() {
            tracing::info!("New active source: {}", source);
            self.active_source = Some(source.to_string());
            self.active_source_since = now;
        }
        self.last_request.insert(source.to_string(), now);

        Ok(())
    }
}

// --- App state ---

type PendingRequests = Arc<Mutex<HashMap<String, oneshot::Sender<Result<(), String>>>>>;

struct AppState {
    worker_stdin: Mutex<Option<tokio::process::ChildStdin>>,
    pending_requests: PendingRequests,
    semaphore: Semaphore,
    render_timeout: Duration,
    worker_alive: Arc<Mutex<bool>>,
    /// Serialises worker spawn attempts so only one runs at a time.
    worker_spawn_lock: Mutex<()>,
    rate_limiter: Mutex<RateLimiter>,
}

// --- Worker management ---

async fn start_worker(state: &Arc<AppState>) -> Result<(), String> {
    let mut child = Command::new("node")
        .arg("pdf-worker.js")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn worker: {e}"))?;

    let stdin = child.stdin.take().ok_or("Failed to get worker stdin")?;
    let stdout = child.stdout.take().ok_or("Failed to get worker stdout")?;
    let stderr = child.stderr.take().ok_or("Failed to get worker stderr")?;

    *state.worker_stdin.lock().await = Some(stdin);

    // Read stderr in background for logging
    tokio::spawn(async move {
        let reader = BufReader::new(stderr);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::warn!(target: "pdf_worker_stderr", "{}", line);
        }
    });

    // Read the ready message with a 15-second startup timeout
    let mut reader = BufReader::new(stdout);
    let mut first_line = String::new();

    match timeout(Duration::from_secs(15), reader.read_line(&mut first_line)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            *state.worker_alive.lock().await = false;
            return Err(format!("Failed to read worker ready message: {e}"));
        }
        Err(_) => {
            *state.worker_alive.lock().await = false;
            return Err("Worker startup timed out after 15s".to_string());
        }
    }

    let msg: WorkerMessage = serde_json::from_str(first_line.trim())
        .map_err(|e| format!("Invalid ready message: {e}"))?;

    match msg {
        WorkerMessage::Ready { ready, error } => {
            if !ready {
                let err = error.unwrap_or_else(|| "Unknown error".to_string());
                *state.worker_alive.lock().await = false;
                return Err(format!("Worker failed to start: {err}"));
            }
        }
        _ => {
            *state.worker_alive.lock().await = false;
            return Err("Unexpected first message from worker".to_string());
        }
    }

    *state.worker_alive.lock().await = true;
    tracing::info!("PDF worker is ready");

    // Spawn stdout reader to dispatch responses
    let pending = state.pending_requests.clone();
    let alive_flag = Arc::clone(&state.worker_alive);

    tokio::spawn(async move {
        let mut lines = reader.lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if let Ok(WorkerMessage::Response {
                        id,
                        success,
                        error,
                    }) = serde_json::from_str::<WorkerMessage>(&line)
                    {
                        let mut map = pending.lock().await;
                        if let Some(tx) = map.remove(&id) {
                            let result = if success {
                                Ok(())
                            } else {
                                Err(error
                                    .unwrap_or_else(|| "Unknown rendering error".to_string()))
                            };
                            let _ = tx.send(result);
                        }
                    }
                }
                Ok(None) => {
                    tracing::error!("Worker stdout closed — worker has exited");
                    *alive_flag.lock().await = false;
                    let mut map = pending.lock().await;
                    for (_, tx) in map.drain() {
                        let _ = tx.send(Err("Worker crashed".to_string()));
                    }
                    break;
                }
                Err(e) => {
                    tracing::error!("Error reading worker stdout: {e}");
                    *alive_flag.lock().await = false;
                    break;
                }
            }
        }
    });

    Ok(())
}

/// Ensures the worker is alive, respawning if needed.
/// Uses a lock to prevent concurrent respawn attempts.
async fn ensure_worker(state: &Arc<AppState>) -> Result<(), String> {
    // Fast path: worker is alive, no lock needed.
    if *state.worker_alive.lock().await {
        return Ok(());
    }

    // Slow path: acquire spawn lock, then double-check.
    let _spawn_guard = state.worker_spawn_lock.lock().await;
    if *state.worker_alive.lock().await {
        return Ok(());
    }

    tracing::info!("Respawning PDF worker...");
    start_worker(state).await
}

// --- Helpers ---

fn extract_client_ip(headers: &HeaderMap, addr: &SocketAddr) -> String {
    // Respect X-Forwarded-For from reverse proxies (Render, Netlify, etc.)
    if let Some(forwarded) = headers.get("x-forwarded-for") {
        if let Ok(val) = forwarded.to_str() {
            if let Some(first) = val.split(',').next() {
                let ip = first.trim();
                if !ip.is_empty() {
                    return ip.to_string();
                }
            }
        }
    }
    addr.ip().to_string()
}

// --- Handlers ---

async fn health_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let alive = *state.worker_alive.lock().await;
    let resp = HealthResponse {
        status: if alive { "ok" } else { "degraded" },
        worker_alive: alive,
    };
    (StatusCode::OK, axum::Json(resp))
}

async fn generate_pdf_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::Json(payload): axum::Json<GeneratePdfRequest>,
) -> impl IntoResponse {
    // Rate limiting
    let source_ip = extract_client_ip(&headers, &addr);
    {
        let mut limiter = state.rate_limiter.lock().await;
        if let Err((status, msg)) = limiter.check(&source_ip) {
            return (status, plain_text_headers(), msg.into_bytes());
        }
    }

    if payload.html.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            plain_text_headers(),
            "Missing or empty 'html' field".into(),
        );
    }

    // Concurrency gate
    let permit = match state.semaphore.try_acquire() {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                plain_text_headers(),
                "Server is at capacity. Try again later.".into(),
            );
        }
    };

    // Ensure worker is alive (respawn if needed)
    if let Err(e) = ensure_worker(&state).await {
        tracing::error!("Cannot start worker: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            plain_text_headers(),
            format!("PDF worker unavailable: {e}").into_bytes(),
        );
    }

    // Create temp files
    let html_file = match tempfile::Builder::new().suffix(".html").tempfile() {
        Ok(f) => f,
        Err(e) => {
            tracing::error!("Failed to create temp HTML file: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                plain_text_headers(),
                "Internal server error".into(),
            );
        }
    };

    let pdf_file = match tempfile::Builder::new().suffix(".pdf").tempfile() {
        Ok(f) => f,
        Err(e) => {
            tracing::error!("Failed to create temp PDF file: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                plain_text_headers(),
                "Internal server error".into(),
            );
        }
    };

    let html_path = html_file.path().to_string_lossy().to_string();
    let pdf_path = pdf_file.path().to_string_lossy().to_string();

    // Write HTML to temp file
    if let Err(e) = std::fs::write(&html_path, &payload.html) {
        tracing::error!("Failed to write HTML to temp file: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            plain_text_headers(),
            "Internal server error".into(),
        );
    }

    // Send request to worker
    let request_id = uuid::Uuid::new_v4().to_string();
    let worker_req = WorkerRequest {
        id: request_id.clone(),
        html_path: html_path.clone(),
        pdf_path: pdf_path.clone(),
    };

    let (tx, rx) = oneshot::channel();
    state
        .pending_requests
        .lock()
        .await
        .insert(request_id.clone(), tx);

    let msg = match serde_json::to_string(&worker_req) {
        Ok(m) => m + "\n",
        Err(e) => {
            tracing::error!("Failed to serialize worker request: {e}");
            state.pending_requests.lock().await.remove(&request_id);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                plain_text_headers(),
                "Internal server error".into(),
            );
        }
    };

    {
        let mut stdin_guard = state.worker_stdin.lock().await;
        if let Some(ref mut stdin) = *stdin_guard {
            if let Err(e) = stdin.write_all(msg.as_bytes()).await {
                tracing::error!("Failed to write to worker stdin: {e}");
                state.pending_requests.lock().await.remove(&request_id);
                *state.worker_alive.lock().await = false;
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    plain_text_headers(),
                    "Failed to communicate with PDF worker".into(),
                );
            }
            if let Err(e) = stdin.flush().await {
                tracing::error!("Failed to flush worker stdin: {e}");
                state.pending_requests.lock().await.remove(&request_id);
                *state.worker_alive.lock().await = false;
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    plain_text_headers(),
                    "Failed to communicate with PDF worker".into(),
                );
            }
        } else {
            state.pending_requests.lock().await.remove(&request_id);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                plain_text_headers(),
                "PDF worker is not available".into(),
            );
        }
    }

    // Wait for response with timeout
    let result = match timeout(state.render_timeout, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => {
            tracing::error!("Worker response channel closed for request {request_id}");
            Err("Worker communication error".to_string())
        }
        Err(_) => {
            tracing::warn!("Render timeout for request {request_id}");
            state.pending_requests.lock().await.remove(&request_id);
            return (
                StatusCode::GATEWAY_TIMEOUT,
                plain_text_headers(),
                "PDF rendering timed out".into(),
            );
        }
    };

    drop(permit);

    match result {
        Ok(()) => match std::fs::read(&pdf_path) {
            Ok(pdf_bytes) => {
                let mut headers = HeaderMap::new();
                headers.insert("content-type", HeaderValue::from_static("application/pdf"));
                headers.insert(
                    "content-disposition",
                    HeaderValue::from_static("attachment; filename=generated-document.pdf"),
                );
                (StatusCode::OK, headers, pdf_bytes)
            }
            Err(e) => {
                tracing::error!("Failed to read generated PDF: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    plain_text_headers(),
                    "Failed to read generated PDF".into(),
                )
            }
        },
        Err(e) => {
            tracing::error!("PDF rendering failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                plain_text_headers(),
                format!("PDF rendering failed: {e}").into_bytes(),
            )
        }
    }
}

fn plain_text_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("text/plain"));
    headers
}

// --- Shutdown ---

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => { tracing::info!("Received Ctrl+C, shutting down..."); }
        _ = terminate => { tracing::info!("Received SIGTERM, shutting down..."); }
    }
}

// --- Main ---

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "html_to_pdf_service=info,tower_http=info".into()),
        )
        .init();

    let config = Config::from_env();

    tracing::info!(
        port = config.port,
        max_concurrent = config.max_concurrent_renders,
        timeout_secs = config.render_timeout_secs,
        "Starting html-to-pdf-service"
    );

    let state = Arc::new(AppState {
        worker_stdin: Mutex::new(None),
        pending_requests: Arc::new(Mutex::new(HashMap::new())),
        semaphore: Semaphore::new(config.max_concurrent_renders),
        render_timeout: Duration::from_secs(config.render_timeout_secs),
        worker_alive: Arc::new(Mutex::new(false)),
        worker_spawn_lock: Mutex::new(()),
        rate_limiter: Mutex::new(RateLimiter::new()),
    });

    if let Err(e) = start_worker(&state).await {
        tracing::error!("Failed to start PDF worker: {e}");
        tracing::warn!(
            "Server will start but PDF generation will be unavailable until worker is respawned"
        );
    }

    // CORS
    let cors = if config.cors_allow_origin == "*" {
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any)
    } else {
        let origin: HeaderValue = config
            .cors_allow_origin
            .parse()
            .expect("Invalid CORS_ALLOW_ORIGIN value");
        CorsLayer::new()
            .allow_origin(origin)
            .allow_methods(Any)
            .allow_headers(Any)
    };

    let app = Router::new()
        .route("/generate-pdf", post(generate_pdf_handler))
        .route("/health", get(health_handler))
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .layer(RequestBodyLimitLayer::new(config.max_body_size_bytes))
        .with_state(state.clone());

    let addr = format!("0.0.0.0:{}", config.port);
    tracing::info!("Listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .unwrap();

    // Clean up: close worker stdin so the Node process exits
    tracing::info!("Shutting down worker...");
    let mut stdin_guard = state.worker_stdin.lock().await;
    if let Some(stdin) = stdin_guard.take() {
        drop(stdin);
    }
}

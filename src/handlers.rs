//! HTTP handlers for the `/generate-pdf` and `/health` endpoints, plus
//! request validation and helper utilities.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::AsyncWriteExt,
    sync::oneshot,
    time::timeout,
};

use crate::state::AppState;
use crate::worker::{WorkerRequest, ensure_worker};

// --- Request / response models ---

fn default_true() -> bool {
    true
}

fn default_format() -> String {
    "A4".to_string()
}

fn default_scale() -> f64 {
    1.0
}

const VALID_FORMATS: &[&str] = &[
    "Letter", "Legal", "Tabloid", "Ledger", "A0", "A1", "A2", "A3", "A4", "A5", "A6",
];

/// JSON body for `POST /generate-pdf`. All fields except `html` are optional.
#[derive(Deserialize)]
pub struct GeneratePdfRequest {
    html: String,
    #[serde(default)]
    landscape: bool,
    #[serde(default = "default_format")]
    format: String,
    #[serde(default = "default_true", rename = "printBackground")]
    print_background: bool,
    #[serde(default = "default_scale")]
    scale: f64,
    #[serde(default, rename = "omitBackground")]
    omit_background: bool,
}

/// JSON response for `GET /health`.
#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    worker_alive: bool,
}

// --- Helpers ---

/// Returns the client IP, reading from `X-Forwarded-For` when `trust_proxy` is true.
fn extract_client_ip(headers: &HeaderMap, addr: &SocketAddr, trust_proxy: bool) -> String {
    if trust_proxy
        && let Some(forwarded) = headers.get("x-forwarded-for")
        && let Ok(val) = forwarded.to_str()
        && let Some(first) = val.split(',').next()
    {
        let ip = first.trim();
        if !ip.is_empty() {
            return ip.to_string();
        }
    }
    addr.ip().to_string()
}

fn plain_text_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("text/plain"));
    headers
}

// --- Handlers ---

/// Returns `{"status":"ok"}` when the worker is alive, `"degraded"` otherwise.
pub async fn health_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let alive = *state.worker_alive.lock().await;
    let resp = HealthResponse {
        status: if alive { "ok" } else { "degraded" },
        worker_alive: alive,
    };
    (StatusCode::OK, axum::Json(resp))
}

/// Accepts HTML and PDF options, renders via the worker, and returns the PDF.
///
/// Pipeline: rate-limit → validate → acquire semaphore → ensure worker →
/// write HTML to temp file → send IPC request → await response → return PDF bytes.
#[allow(clippy::too_many_lines)] // Linear request pipeline; splitting would scatter single-caller logic.
pub async fn generate_pdf_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::Json(payload): axum::Json<GeneratePdfRequest>,
) -> impl IntoResponse {
    // Rate limiting
    let source_ip = extract_client_ip(&headers, &addr, state.trust_proxy);
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

    if payload.html.len() > state.max_html_size {
        return (
            StatusCode::BAD_REQUEST,
            plain_text_headers(),
            "HTML content too large".into(),
        );
    }

    if !(0.1..=2.0).contains(&payload.scale) {
        return (
            StatusCode::BAD_REQUEST,
            plain_text_headers(),
            "Scale must be between 0.1 and 2.0".into(),
        );
    }

    if !VALID_FORMATS.iter().any(|f| f.eq_ignore_ascii_case(&payload.format)) {
        return (
            StatusCode::BAD_REQUEST,
            plain_text_headers(),
            format!("Invalid format. Valid options: {}", VALID_FORMATS.join(", ")).into(),
        );
    }

    // Concurrency gate
    let Ok(permit) = state.semaphore.try_acquire() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            plain_text_headers(),
            "Server is at capacity. Try again later.".into(),
        );
    };

    // Ensure worker is alive (respawn if needed)
    if let Err(e) = ensure_worker(&state).await {
        tracing::error!("Cannot start worker: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            plain_text_headers(),
            "PDF service is temporarily unavailable. Please try again later.".into(),
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

    // Grab paths before moving on — the NamedTempFiles must stay alive (not
    // dropped) until the render completes, otherwise the OS deletes them.
    let html_path = html_file.path().to_string_lossy().to_string();
    let pdf_path = pdf_file.path().to_string_lossy().to_string();

    // Write HTML to temp file
    if let Err(e) = tokio::fs::write(&html_path, &payload.html).await {
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
        landscape: payload.landscape,
        format: payload.format,
        print_background: payload.print_background,
        scale: payload.scale,
        omit_background: payload.omit_background,
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

    // Release the semaphore slot before the disk read so we don't hold a
    // render permit while doing I/O that doesn't need Chrome.
    drop(permit);

    match result {
        Ok(()) => match tokio::fs::read(&pdf_path).await {
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
                "PDF rendering failed. Please check your HTML and try again.".into(),
            )
        }
    }
}

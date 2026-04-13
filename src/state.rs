//! Shared application state held in an `Arc` and passed to every handler.

use std::{collections::HashMap, sync::Arc};

use tokio::{
    sync::{Mutex, Semaphore, oneshot},
    time::{Duration, Instant},
};

use crate::rate_limiter::RateLimiter;

/// In-flight render requests awaiting a response from the worker, keyed by request ID.
pub type PendingRequests = Arc<Mutex<HashMap<String, oneshot::Sender<Result<(), String>>>>>;

/// Shared state for the Axum application.
///
/// Wrapped in `Arc` and cloned into every handler via Axum's `State` extractor.
pub struct AppState {
    /// Pipe to the worker's stdin for sending render requests.
    pub worker_stdin: Mutex<Option<tokio::process::ChildStdin>>,
    /// Render requests waiting for a worker response.
    pub pending_requests: PendingRequests,
    /// Limits concurrent renders to prevent resource exhaustion.
    pub semaphore: Semaphore,
    /// Maximum time to wait for a single render before returning 504.
    pub render_timeout: Duration,
    /// Whether the worker process is currently alive and accepting requests.
    pub worker_alive: Arc<Mutex<bool>>,
    /// Serialises worker spawn attempts so only one runs at a time.
    pub worker_spawn_lock: Mutex<()>,
    /// Global rate limiter (30-second cooldown between requests).
    pub rate_limiter: Mutex<RateLimiter>,
    /// Maximum allowed HTML payload size in bytes.
    pub max_html_size: usize,
    /// Tracks the last worker spawn attempt to enforce a cooldown between retries.
    pub last_spawn_attempt: Mutex<Option<Instant>>,
    /// Server start time, used to compute uptime in the health endpoint.
    pub started_at: Instant,
    /// Total render slots (mirrors the semaphore capacity for health reporting).
    pub max_concurrent: usize,
}

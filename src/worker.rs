//! Node.js worker process management and IPC.
//!
//! The worker (`pdf-worker.ts`, compiled to `dist/pdf-worker.js`) maintains a
//! persistent headless Chrome instance. Communication is newline-delimited JSON
//! over stdin (requests) and stdout (responses).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    time::{Duration, Instant, timeout},
};

use crate::state::AppState;

// --- IPC types ---

/// A render request serialized as JSON and sent to the worker via stdin.
///
/// Field names are camelCase in JSON (via `#[serde(rename)]`) to match the
/// TypeScript `WorkerRequest` interface in `pdf-worker.ts`.
#[derive(Serialize)]
pub struct WorkerRequest {
    pub id: String,
    #[serde(rename = "htmlPath")]
    pub html_path: String,
    #[serde(rename = "pdfPath")]
    pub pdf_path: String,
    pub landscape: bool,
    pub format: String,
    #[serde(rename = "printBackground")]
    pub print_background: bool,
    pub scale: f64,
    #[serde(rename = "omitBackground")]
    pub omit_background: bool,
}

/// A message received from the worker via stdout, tagged by `"type"`.
#[derive(Deserialize)]
#[serde(tag = "type")]
enum WorkerMessage {
    #[serde(rename = "ready")]
    Ready {
        ready: bool,
        #[serde(default)]
        error: Option<String>,
    },
    #[serde(rename = "response")]
    Response {
        id: String,
        success: bool,
        #[serde(default)]
        error: Option<String>,
    },
}

// --- Worker lifecycle ---

/// Spawns the Node.js worker, waits for its `"ready"` message, and starts a
/// background task to dispatch render responses back to waiting handlers.
pub async fn start_worker(state: &Arc<AppState>) -> Result<(), String> {
    let mut child = Command::new("node")
        .arg("dist/pdf-worker.js")
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

    if let WorkerMessage::Ready { ready, error } = msg {
        if !ready {
            let err = error.unwrap_or_else(|| "Unknown error".to_string());
            *state.worker_alive.lock().await = false;
            return Err(format!("Worker failed to start: {err}"));
        }
    } else {
        *state.worker_alive.lock().await = false;
        return Err("Unexpected first message from worker".to_string());
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
/// Uses a lock to prevent concurrent respawn attempts and a cooldown to avoid
/// rapid crash-loop spawning (e.g., when Chrome binary is missing).
pub async fn ensure_worker(state: &Arc<AppState>) -> Result<(), String> {
    // Fast path: worker is alive, no lock needed.
    if *state.worker_alive.lock().await {
        return Ok(());
    }

    // Slow path: acquire spawn lock, then double-check.
    let _spawn_guard = state.worker_spawn_lock.lock().await;
    if *state.worker_alive.lock().await {
        return Ok(());
    }

    // Enforce a 10-second cooldown between spawn attempts to prevent rapid retries.
    {
        let mut last_attempt = state.last_spawn_attempt.lock().await;
        if let Some(last) = *last_attempt {
            let elapsed = Instant::now().duration_since(last);
            if elapsed < Duration::from_secs(10) {
                let wait = 10 - elapsed.as_secs();
                return Err(format!(
                    "Worker respawn on cooldown. Try again in ~{wait} seconds."
                ));
            }
        }
        *last_attempt = Some(Instant::now());
    }

    tracing::info!("Respawning PDF worker...");
    start_worker(state).await
}

//! IP-based rate limiting with a two-tier scheme:
//!
//! 1. **Single-source lock** — only one IP may use the API per hour.
//! 2. **Per-source cooldown** — the active IP must wait 30 seconds between requests.
//!
//! See the README "Rate Limiting" section for the full behaviour matrix.

use std::collections::HashMap;

use axum::http::StatusCode;
use tokio::time::{Duration, Instant};

/// Enforces single-source locking and per-source cooldown.
///
/// All state is in-memory; it resets on restart.
pub struct RateLimiter {
    /// The single source (IP) that currently "owns" the API for this hour window.
    active_source: Option<String>,
    /// When the active source's hour window started.
    active_source_since: Instant,
    /// Last request time per source, for the 30-second cooldown.
    last_request: HashMap<String, Instant>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            active_source: None,
            active_source_since: Instant::now(),
            last_request: HashMap::new(),
        }
    }

    /// Check if a source is allowed to make a request.
    /// Returns Ok(()) or Err with a human-readable reason.
    pub fn check(&mut self, source: &str) -> Result<(), (StatusCode, String)> {
        let now = Instant::now();

        // Clean up expired active source (1-hour window)
        if let Some(ref active) = self.active_source
            && now.duration_since(self.active_source_since) > Duration::from_secs(3600)
        {
            tracing::info!("Active source '{}' expired after 1 hour", active);
            self.active_source = None;
            self.last_request.clear();
        }

        // If there's an active source, check the 30-second cooldown first
        // using the active source's last request time. This runs before the
        // single-source check so the user sees a helpful "wait N seconds"
        // message even if their IP varies slightly between proxy hops.
        if let Some(ref active) = self.active_source {
            if let Some(last) = self.last_request.get(active) {
                let elapsed = now.duration_since(*last);
                if elapsed < Duration::from_secs(30) {
                    let wait = 30 - elapsed.as_secs();
                    return Err((
                        StatusCode::TOO_MANY_REQUESTS,
                        format!("Rate limited. Please wait {wait} seconds."),
                    ));
                }
            }

            // Past cooldown — now check if this is a different source
            if active != source {
                let remaining = Duration::from_secs(3600)
                    .saturating_sub(now.duration_since(self.active_source_since));
                let mins = remaining.as_secs() / 60;
                return Err((
                    StatusCode::TOO_MANY_REQUESTS,
                    format!(
                        "API is currently locked to another source. Try again in ~{} minutes.",
                        mins + 1
                    ),
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

    /// Returns the current rate limiter state for the health endpoint.
    ///
    /// Reports whether the API is locked to a source and, if so, how many
    /// seconds remain. Does **not** expose the active source IP.
    pub fn status(&self) -> (bool, u64) {
        match &self.active_source {
            Some(_) => {
                let elapsed = Instant::now().duration_since(self.active_source_since);
                let remaining = Duration::from_secs(3600).saturating_sub(elapsed);
                (true, remaining.as_secs())
            }
            None => (false, 0),
        }
    }
}

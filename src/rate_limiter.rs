//! Global rate limiting with a 30-second cooldown between requests.
//!
//! Any request made within 30 seconds of the previous one is rejected with
//! a `429 Too Many Requests` response.

use axum::http::StatusCode;
use tokio::time::{Duration, Instant};

/// Enforces a global 30-second cooldown between requests.
///
/// All state is in-memory; it resets on restart.
pub struct RateLimiter {
    /// When the last successful request was admitted.
    last_request: Option<Instant>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            last_request: None,
        }
    }

    /// Check if a request is allowed.
    /// Returns Ok(()) or Err with status, message, and seconds to wait.
    pub fn check(&mut self) -> Result<(), (StatusCode, String, u64)> {
        let now = Instant::now();

        if let Some(last) = self.last_request {
            let elapsed = now.duration_since(last);
            if elapsed < Duration::from_secs(30) {
                let wait = Self::remaining_secs(elapsed);
                return Err((
                    StatusCode::TOO_MANY_REQUESTS,
                    format!("Rate limited. Please wait {wait} seconds."),
                    wait,
                ));
            }
        }

        self.last_request = Some(now);
        Ok(())
    }

    /// Returns the number of seconds remaining in the cooldown (0 when ready).
    pub fn status(&self) -> u64 {
        match self.last_request {
            Some(last) => Self::remaining_secs(Instant::now().duration_since(last)),
            None => 0,
        }
    }

    /// Seconds remaining in the 30-second cooldown, rounded up so callers
    /// never see 0 while the cooldown is still active.
    fn remaining_secs(elapsed: Duration) -> u64 {
        let remaining = Duration::from_secs(30).saturating_sub(elapsed);
        remaining.as_secs() + if remaining.subsec_nanos() > 0 { 1 } else { 0 }
    }
}

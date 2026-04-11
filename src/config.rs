//! Environment-based configuration for the service.
//!
//! Every setting has a sensible default and can be overridden via an
//! environment variable. See the README "Configuration" table for details.

/// Server configuration parsed from environment variables at startup.
pub struct Config {
    pub port: u16,
    pub max_concurrent_renders: usize,
    pub render_timeout_secs: u64,
    pub max_body_size_bytes: usize,
    pub cors_allow_origin: String,
    pub trust_proxy: bool,
}

impl Config {
    pub fn from_env() -> Self {
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
            trust_proxy: std::env::var("TRUST_PROXY")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false),
        }
    }
}

//! Entry point for the html-to-pdf-service.
//!
//! Wires together configuration, middleware, routing, and graceful shutdown.
//! All business logic lives in the [`handlers`], [`worker`], and
//! [`rate_limiter`] modules.

mod config;
mod handlers;
mod rate_limiter;
mod state;
mod worker;

use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use axum::{
    http::{HeaderValue, Method},
    routing::{get, post},
    Router,
};
use tokio::{
    signal,
    sync::{Mutex, Semaphore},
    time::Duration,
};
use tower_http::{
    cors::{Any, CorsLayer},
    limit::RequestBodyLimitLayer,
    set_header::SetResponseHeaderLayer,
    trace::TraceLayer,
};

use config::Config;
use rate_limiter::RateLimiter;
use state::AppState;

/// Builds a [`CorsLayer`] from the configured origin, falling back to wildcard on parse error.
fn build_cors(origin: &str) -> CorsLayer {
    let methods = [Method::GET, Method::POST, Method::OPTIONS];
    let headers = [axum::http::header::CONTENT_TYPE];

    if origin == "*" {
        return CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(methods)
            .allow_headers(headers);
    }

    match origin.parse::<HeaderValue>() {
        Ok(value) => CorsLayer::new()
            .allow_origin(value)
            .allow_methods(methods)
            .allow_headers(headers),
        Err(e) => {
            tracing::error!("Invalid CORS_ALLOW_ORIGIN '{origin}': {e}. Falling back to wildcard.");
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(methods)
                .allow_headers(headers)
        }
    }
}

/// Waits for Ctrl+C or SIGTERM, then returns to trigger graceful shutdown.
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
        () = ctrl_c => { tracing::info!("Received Ctrl+C, shutting down..."); }
        () = terminate => { tracing::info!("Received SIGTERM, shutting down..."); }
    }
}

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
        trust_proxy = config.trust_proxy,
        "Starting html-to-pdf-service"
    );

    if config.trust_proxy {
        tracing::info!(
            "TRUST_PROXY enabled: rate limiting uses real client IPs from X-Forwarded-For. \
            Single-source 1-hour lock is active."
        );
    } else {
        tracing::info!(
            "TRUST_PROXY disabled: rate limiting uses socket address. \
            On proxied deployments, the 30-second cooldown acts as a global throttle."
        );
    }

    let state = Arc::new(AppState {
        worker_stdin: Mutex::new(None),
        pending_requests: Arc::new(Mutex::new(HashMap::new())),
        semaphore: Semaphore::new(config.max_concurrent_renders),
        render_timeout: Duration::from_secs(config.render_timeout_secs),
        worker_alive: Arc::new(Mutex::new(false)),
        worker_spawn_lock: Mutex::new(()),
        rate_limiter: Mutex::new(RateLimiter::new()),
        trust_proxy: config.trust_proxy,
        max_html_size: config.max_body_size_bytes,
        last_spawn_attempt: Mutex::new(None),
        started_at: tokio::time::Instant::now(),
        max_concurrent: config.max_concurrent_renders,
    });

    if let Err(e) = worker::start_worker(&state).await {
        tracing::error!("Failed to start PDF worker: {e}");
        tracing::warn!(
            "Server will start but PDF generation will be unavailable until worker is respawned"
        );
    }

    if config.cors_allow_origin == "*" {
        tracing::warn!(
            "CORS is configured with wildcard origin '*'. \
             Consider setting CORS_ALLOW_ORIGIN to a specific origin in production."
        );
    }
    let cors = build_cors(&config.cors_allow_origin);

    let app = Router::new()
        .route("/generate-pdf", post(handlers::generate_pdf_handler))
        .route("/health", get(handlers::health_handler))
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .layer(RequestBodyLimitLayer::new(config.max_body_size_bytes))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'self'; script-src 'none'; style-src 'unsafe-inline'",
            ),
        ))
        .with_state(state.clone());

    let addr = format!("0.0.0.0:{}", config.port);
    tracing::info!("Listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("Failed to bind to {addr}: {e}. Is the port already in use?"));

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .expect("Server encountered a fatal error");

    // Clean up: close worker stdin so the Node process exits
    tracing::info!("Shutting down worker...");
    let mut stdin_guard = state.worker_stdin.lock().await;
    if let Some(stdin) = stdin_guard.take() {
        drop(stdin);
    }
}

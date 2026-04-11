# HTML to PDF Conversion Service

A high-performance backend service that converts raw HTML into formatted PDF documents. Built with a Rust (axum) web server and a persistent Node.js worker running headless Chrome via puppeteer-core.

## Architecture

- **Rust server** handles HTTP requests, concurrency control, and temp file management
- **Node.js worker** maintains a warm headless Chrome instance for fast PDF rendering
- Communication between Rust and Node.js via stdin/stdout newline-delimited JSON

```
src/
├── main.rs            Entry point, router, middleware, shutdown
├── config.rs          Environment variable parsing
├── state.rs           Shared application state
├── rate_limiter.rs    IP-based rate limiting
├── worker.rs          Node.js worker IPC and lifecycle
├── handlers.rs        HTTP handlers and request validation
pdf-worker.ts          Headless Chrome PDF rendering (TypeScript)
```

## Prerequisites

- [Rust](https://rustup.rs/) 1.94+ (edition 2024)
- [Node.js](https://nodejs.org/) v20+
- Google Chrome or Chromium installed locally

## Setup

```bash
# Install Node.js dependencies
npm install

# Build the TypeScript worker
npm run build

# Build and run the server
cargo run
```

The server starts on port **3001** by default.

If Chrome is not in a standard location, set the path explicitly:

```bash
CHROME_EXECUTABLE_PATH="/path/to/chrome" cargo run
```

## API

### `POST /generate-pdf`

Converts HTML to a PDF document.

**Request:**
```bash
curl -X POST http://localhost:3001/generate-pdf \
  -H "Content-Type: application/json" \
  -d '{"html": "<h1>Hello</h1><p>World</p>"}' \
  -o output.pdf
```

**Request with PDF options:**
```bash
curl -X POST http://localhost:3001/generate-pdf \
  -H "Content-Type: application/json" \
  -d '{
    "html": "<h1>Hello</h1><p>World</p>",
    "landscape": true,
    "format": "Letter",
    "printBackground": true,
    "scale": 0.8,
    "omitBackground": false
  }' \
  -o output.pdf
```

All PDF options are optional. When omitted, defaults match the original behavior.

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `landscape` | boolean | `false` | Page orientation |
| `format` | string | `"A4"` | Page size: Letter, Legal, Tabloid, Ledger, A0-A6 |
| `printBackground` | boolean | `true` | Include CSS background graphics |
| `scale` | number | `1` | Page scale factor (0.1 - 2.0) |
| `omitBackground` | boolean | `false` | Omit the default white page background |

**Response:** Binary PDF with `Content-Type: application/pdf`

### `GET /health`

Returns service health status.

```json
{ "status": "ok", "worker_alive": true }
```

Returns `"status": "degraded"` when the worker process is down.

## Configuration

All settings are configured via environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `PORT` | `3001` | Server listen port (Render sets this automatically) |
| `CHROME_EXECUTABLE_PATH` | auto-detect | Path to Chrome/Chromium binary |
| `MAX_CONCURRENT_RENDERS` | `4` | Max simultaneous PDF renders |
| `RENDER_TIMEOUT_SECS` | `30` | Timeout per render request (seconds) |
| `MAX_BODY_SIZE_BYTES` | `5242880` | Max request body size (5 MB) |
| `CORS_ALLOW_ORIGIN` | `*` | Allowed CORS origin (a warning is logged when using wildcard) |
| `TRUST_PROXY` | `false` | Set to `true` to read client IP from `X-Forwarded-For` header (enable when behind a reverse proxy) |

## Deploying to Render

1. Create a new **Web Service** on Render
2. Set the **Environment** to **Docker**
3. Set the **Health Check Path** to `/health`
4. Configure environment variables in the Render dashboard:
   - `MAX_CONCURRENT_RENDERS` — tune based on instance RAM (2 for 1GB, 4 for 2GB)
   - `CORS_ALLOW_ORIGIN` — set to your frontend domain
5. Use the **Starter plan** (1 GB RAM) or higher — Chrome uses ~200-300 MB

## Rate Limiting

The API enforces IP-based rate limiting to prevent abuse while remaining open for multiple client applications:

- **Single-source lock:** Only one IP address can use the API at a time. Once a source makes a request, that IP has exclusive access for **1 hour**. Any other IP receives a `429` response until the lock expires.
- **Per-source cooldown:** The active source must wait **30 seconds** between requests. Requests made within the cooldown window receive a `429` response.
- **Proxy support:** When `TRUST_PROXY=true`, the server reads the client IP from the `X-Forwarded-For` header. This should only be enabled when deployed behind a trusted reverse proxy (e.g., Render, Nginx). When disabled (default), the direct socket address is always used.

**Behaviour depends on `TRUST_PROXY`:**

- `TRUST_PROXY=false` (default): On proxied platforms, all clients share the same apparent IP. The 1-hour lock is bypassed and the 30-second cooldown becomes a global throttle across all users.
- `TRUST_PROXY=true`: Real client IPs are used. The 1-hour single-source lock is enforced per IP, blocking all other clients for the duration of the window.

Both are valid patterns. Set `TRUST_PROXY` according to your intended access policy.

## Error Responses

| Status | Condition |
|--------|-----------|
| 400 | Malformed JSON, missing or empty `html` field, invalid `format`, or `scale` out of range |
| 413 | Payload exceeds body size limit |
| 429 | Another source is currently active (single-source lock) |
| 429 | Request sent within 30-second cooldown window |
| 429 | All rendering slots busy (concurrency limit reached) |
| 500 | Worker crash, file system error, or rendering failure |
| 504 | Render timeout exceeded |

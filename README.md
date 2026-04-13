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
├── rate_limiter.rs    Global rate limiting
├── worker.rs          Node.js worker IPC and lifecycle
├── handlers.rs        HTTP handlers and request validation
pdf-worker.ts          Headless Chrome PDF rendering (TypeScript)
sync-version.js        Syncs package.json version from Cargo.toml
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
| `format` | string | `"A4"` | Page size (case-insensitive): Letter, Legal, Tabloid, Ledger, A0-A6 |
| `printBackground` | boolean | `true` | Include CSS background graphics |
| `scale` | number | `1` | Page scale factor (0.1 - 2.0) |
| `omitBackground` | boolean | `false` | Omit the default white page background |

**Response:** Binary PDF with `Content-Type: application/pdf`

### `GET /health`

Returns service health status.

```json
{
  "status": "ok",
  "version": "<cargo_version>",
  "uptime_secs": 3600,
  "worker_alive": true,
  "renders": { "available": 3, "max": 4 },
  "rate_limiter": { "cooldown_remaining_secs": 0 }
}
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

## Deploying to Render

1. Create a new **Web Service** on Render
2. Set the **Environment** to **Docker**
3. Set the **Health Check Path** to `/health`
4. Configure environment variables in the Render dashboard:
   - `MAX_CONCURRENT_RENDERS` — tune based on instance RAM (2 for 1GB, 4 for 2GB)
   - `CORS_ALLOW_ORIGIN` — set to your frontend domain
5. Use the **Starter plan** (1 GB RAM) or higher — Chrome uses ~200-300 MB

## Rate Limiting

The API enforces a global **30-second cooldown** between requests. Any request made within 30 seconds of the previous one receives a `429 Too Many Requests` response with the number of seconds to wait. The cooldown is in-memory and resets on restart.

Successful (`200`) and rate-limited (`429`) responses include standard rate limit headers:

| Header | Description |
|--------|-------------|
| `X-RateLimit-Limit` | Maximum requests per 30-second window (`1`) |
| `X-RateLimit-Remaining` | Requests remaining in the current window |
| `X-RateLimit-Reset` | Seconds until the cooldown expires (relative, not a Unix timestamp) |

Rate-limited `429` responses additionally include a `Retry-After` header with the number of seconds to wait before retrying.

## Security

- **No JavaScript execution:** The renderer runs with JavaScript disabled. Inline `<script>` tags and JS event handlers in the submitted HTML will not execute. Only static HTML and CSS are rendered.
- **SSRF protection:** All external network requests from the rendered page are blocked. Only inline data URIs and `about:blank` are allowed.
- **Request isolation:** Each render gets its own Chrome page with request interception enabled.

## Error Responses

| Status | Condition |
|--------|-----------|
| 400 | Malformed JSON, missing or empty `html` field, invalid `format`, or `scale` out of range |
| 413 | Payload exceeds body size limit |
| 429 | Request sent within 30-second cooldown window |
| 429 | All rendering slots busy (concurrency limit reached) |
| 500 | Worker crash, file system error, or rendering failure |
| 504 | Render timeout exceeded |

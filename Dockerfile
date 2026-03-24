# Stage 1: Build the Rust binary
FROM rust:1.85-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src/ src/

RUN cargo build --release

# Stage 2: Get Node.js from official image
FROM node:20-slim AS node

# Stage 3: Runtime
FROM debian:bookworm-slim

# Install Chromium and required system libraries
RUN apt-get update && apt-get install -y --no-install-recommends \
    chromium \
    ca-certificates \
    fonts-liberation \
    fonts-noto-color-emoji \
    libasound2 \
    libatk-bridge2.0-0 \
    libatk1.0-0 \
    libcups2 \
    libdbus-1-3 \
    libdrm2 \
    libgbm1 \
    libgtk-3-0 \
    libnspr4 \
    libnss3 \
    libxcomposite1 \
    libxdamage1 \
    libxrandr2 \
    && rm -rf /var/lib/apt/lists/*

# Copy Node.js from the official image (avoids Debian's outdated nodejs package)
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s /usr/local/lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm

# Create non-root user
RUN groupadd -r appuser && useradd -r -g appuser -d /app -s /sbin/nologin appuser

WORKDIR /app

# Install Node.js dependencies
COPY package.json package-lock.json* ./
RUN npm ci --omit=dev && npm cache clean --force

# Copy application files
COPY pdf-worker.js ./
COPY --from=builder /app/target/release/html-to-pdf-service ./

# Set ownership and switch to non-root user
RUN chown -R appuser:appuser /app
USER appuser

ENV CHROME_EXECUTABLE_PATH=/usr/bin/chromium

EXPOSE 3001

CMD ["./html-to-pdf-service"]

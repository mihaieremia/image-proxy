# Multi-stage build for the native image-proxy server.
#
# Build:  docker build -t image-proxy .
# Run:    docker run -p 8080:8080 -e ALLOWED_DOMAINS=cdn.example.com image-proxy
#
# Environment variables (all optional):
#   PORT              - Listen port (default: 8080)
#   MAX_WIDTH         - Max output width (default: 4096)
#   MAX_HEIGHT        - Max output height (default: 4096)
#   MAX_SIZE_MB       - Max source image size in MB (default: 25)
#   CACHE_TTL         - Cache-Control max-age in seconds (default: 7776000)
#   ALLOWED_ORIGINS   - Comma-separated allowed request origins
#   ALLOWED_DOMAINS   - Comma-separated allowed source image domains
#   UPSTREAM_REFERER  - Referer header for upstream requests

# --- Builder stage ---
FROM rust:latest AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

# Build the native server binary (not the WASM Worker)
RUN cargo build --release \
    --features native \
    --no-default-features \
    --bin image-proxy-server

# --- Runtime stage ---
FROM debian:bookworm-slim

# Install CA certificates for HTTPS upstream requests
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/image-proxy-server /usr/local/bin/

EXPOSE 8080

# Non-root user for security
RUN useradd -r -s /bin/false proxy
USER proxy

CMD ["image-proxy-server"]

# Image Proxy

High-performance image resizing proxy running on Cloudflare Workers. Written in Rust, compiled to WASM. Zero cold starts, global edge deployment.

Built for [Chartex](https://chartex.com).

## Features

- **On-the-fly resizing** — Resize any image via query params (`w`, `h`, `fit`)
- **Quality control** — Adjustable JPEG quality via `q` param (1-100)
- **Smart format selection** — JPEG stays JPEG (lossy), PNG becomes WebP (lossless), GIF passes through (preserves animation)
- **Edge caching** — 90-day cache with path-based keys (auth tokens stripped)
- **Three-layer security** — Origin check + SSRF protection + source domain allowlist
- **CDN-compatible fetching** — Browser-like headers for upstream requests
- **Dual target** — Cloudflare Worker (WASM) or native binary (Docker) from the same codebase
- **Pure Rust** — No FFI, no native dependencies, instant deploys

## API

```
GET /resize?url={encoded-url}&w={width}&h={height}&q={quality}&fit={mode}
GET /?url={encoded-url}&w={width}&h={height}&q={quality}&fit={mode}
```

### Parameters

| Parameter | Required | Default | Description |
|-----------|----------|---------|-------------|
| `url` | Yes | — | **Percent-encoded** source image URL |
| `w` / `width` | No | — | Target width (1-4096) |
| `h` / `height` | No | — | Target height (1-4096) |
| `q` / `quality` | No | 80 | JPEG output quality (1-100). Only applies to JPEG inputs. |
| `fit` | No | `scale-down` | Resize mode: `scale-down`, `cover`, `contain`, `crop` |

### Fit Modes

| Mode | Behavior |
|------|----------|
| `scale-down` | Shrink to fit within dimensions, never upscale |
| `contain` | Fit within dimensions, may upscale |
| `cover` | Fill dimensions exactly, crop overflow |
| `crop` | Center-crop to exact dimensions |

### Output Format

| Input | Output | Why |
|-------|--------|-----|
| JPEG | JPEG (lossy) | Quality controllable via `q`, no lossless→lossless size blowup |
| PNG | WebP (lossless) | Typically 30-50% smaller than PNG |
| GIF | Passthrough | Preserves animation (decoding would destroy frames) |
| WebP (no resize) | Passthrough | Already optimal format |

### URL Encoding

The source URL **must** be percent-encoded. Raw `&` characters in the source URL will be interpreted as proxy parameter separators.

```javascript
// Correct
const src = `https://proxy.example.com/resize?url=${encodeURIComponent(imageUrl)}&w=300&q=80`;

// Wrong — query params in source URL will break
const src = `https://proxy.example.com/resize?url=${imageUrl}&w=300`;
```

### Response Headers

| Header | Value |
|--------|-------|
| `Content-Type` | `image/jpeg`, `image/webp`, or source type (passthrough) |
| `Cache-Control` | `public, immutable, no-transform, max-age=7776000, s-maxage=7776000` |
| `X-Image-Original-Size` | Source image size in bytes |
| `X-Image-Output-Size` | Output image size in bytes |
| `Access-Control-Allow-Origin` | Matched origin (not `*`) |
| `Vary` | `Origin` |

### Error Responses

| Status | Condition |
|--------|-----------|
| 400 | Missing `url` param, invalid dimensions |
| 403 | Origin not allowed, source domain not allowed, SSRF blocked |
| 413 | Source image exceeds size limit |
| 415 | Source is not a supported image type |
| 422 | Image decode or encode failed |
| 502 | Upstream fetch failed |

### Supported Input Formats

JPEG, PNG, GIF, WebP

## Security

### Origin Validation

Only requests from allowed origins are served. Configured via `ALLOWED_ORIGINS`:

```toml
[vars]
ALLOWED_ORIGINS = "https://chartex.com,https://www.chartex.com"
```

- `Origin` header checked first (browsers send this on cross-origin requests)
- Falls back to `Referer` header
- Requests without either header are allowed (cURL, same-origin `<img>`, server-to-server) — source domain allowlist still applies

### SSRF Protection

Requests to private/reserved IP ranges are blocked:

- Loopback: `127.0.0.0/8`, `::1`
- Private: `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`
- Link-local: `169.254.0.0/16` (includes cloud metadata endpoints)
- CGN: `100.64.0.0/10`
- Reserved hostnames: `localhost`, `metadata.google.internal`
- Non-HTTP(S) schemes blocked

### Source Domain Allowlist

Only images from whitelisted domains can be proxied:

```toml
[vars]
ALLOWED_DOMAINS = "p16-common-sign.tiktokcdn-eu.com,cdn.example.com"
```

- Supports subdomains: `example.com` allows `cdn.example.com`
- Empty or unset = all domains allowed (not recommended for production)

## Caching

- **Cloudflare Cache API** — Edge-cached per region, globally distributed
- **Path-based cache keys** — Query parameters (auth tokens, signatures, expiry) are stripped from the source URL. Rotating signed URLs for the same image hit a single cache entry
- **Quality-aware** — Cache key includes quality and fit mode, so `q=80` and `q=60` are cached separately
- **Configurable TTL** — Default 90 days, set via `CACHE_TTL` env var (seconds)

## Configuration

| Variable | Default | Description |
|---|---|---|
| `ALLOWED_ORIGINS` | `https://chartex.com,https://www.chartex.com` | Origins allowed to call the proxy |
| `ALLOWED_DOMAINS` | *(empty = all)* | Source image domains allowed to be proxied |
| `UPSTREAM_REFERER` | `https://chartex.com/` | Referer header sent to upstream CDNs |
| `MAX_WIDTH` | `4096` | Maximum output width in pixels |
| `MAX_HEIGHT` | `4096` | Maximum output height in pixels |
| `MAX_SIZE_MB` | `25` | Maximum source image size in MB |
| `CACHE_TTL` | `7776000` | Cache TTL in seconds (default: 90 days) |

All variables are set in `wrangler.toml` under `[vars]` (Cloudflare) or as environment variables (native/Docker).

## Deployment

### Option A: Cloudflare Worker (WASM)

```bash
# Setup
rustup target add wasm32-unknown-unknown
npm install

# Development
npx wrangler dev

# Deploy
npx wrangler deploy

# Verify
cargo check --target wasm32-unknown-unknown
cargo clippy --target wasm32-unknown-unknown
```

### Option B: Native Binary (Docker)

```bash
# Build and run
docker build -t image-proxy .
docker run -p 8080:8080 \
  -e ALLOWED_DOMAINS=cdn.example.com \
  -e ALLOWED_ORIGINS=https://chartex.com \
  image-proxy

# Or build directly
cargo build --release --features native --no-default-features --bin image-proxy-server
./target/release/image-proxy-server
```

The native server listens on port 8080 (configurable via `PORT` env var) and provides the same API as the Cloudflare Worker, minus edge caching.

## Architecture

```
src/
├── lib.rs         # Cloudflare Worker entry point (cfg cloudflare)
├── native.rs      # Native axum/tokio server (cfg native, [[bin]])
├── config.rs      # Config struct from worker env vars (cfg cloudflare)
├── cors.rs        # Origin validation + CORS headers (cfg cloudflare)
├── handler.rs     # Worker request pipeline (cfg cloudflare)
├── security.rs    # Shared: domain allowlist, SSRF protection, content-type validation
├── params.rs      # Shared: query param parsing, cache key generation
├── process.rs     # Shared: decode → resize → encode (JPEG/WebP)
└── error.rs       # Shared: ProxyError enum → HTTP status codes
```

```
Client Request
     │
     ▼
┌──────────────┐
│  lib.rs /    │  Route: OPTIONS → preflight, / | /resize → handler
│  native.rs   │
└──────┬───────┘
       ▼
┌──────────────┐
│  cors.rs /   │  Validate Origin/Referer against ALLOWED_ORIGINS
│  native.rs   │
└──────┬───────┘
       ▼
┌──────────────┐
│  security.rs │  SSRF check → domain allowlist (shared)
└──────┬───────┘
       ▼
┌──────────────┐
│  handler.rs /│  Cache lookup → GET upstream → validate → process
│  native.rs   │
└──────┬───────┘
       ▼
┌──────────────┐
│  process.rs  │  Format decision → Decode → Resize (CatmullRom) → Encode
└──────┬───────┘
       ▼
  Response (cached at edge for Worker, no cache for native)
```

### Technical Decisions

- **Dual target** — Same codebase compiles to WASM (Cloudflare Worker) or native binary (Docker). Shared modules: `security.rs`, `params.rs`, `process.rs`, `error.rs`.
- **No HEAD requests** — Many CDNs reject HEAD (TikTok, etc.). GET validates everything.
- **JPEG→JPEG, not JPEG→WebP** — `image-webp` only supports lossless encoding, which produces larger files than the JPEG input. JPEG re-encoding with quality control gives actual size reduction.
- **CatmullRom filter** — Bicubic resampling instead of Lanczos3. Visually identical for web images, ~2x faster.
- **Memory-conscious** — Source bytes dropped before resize, pixel buffers extracted with `into_raw()` (zero-copy), output buffers pre-allocated.

## License

Private. All rights reserved.

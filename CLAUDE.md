# Image Proxy — Cloudflare Worker (Rust/WASM)

## Build & Deploy

```bash
# Check compilation for WASM target
cargo check --target wasm32-unknown-unknown

# Lint
cargo clippy --target wasm32-unknown-unknown

# Local dev server
npx wrangler dev

# Deploy to Cloudflare
npx wrangler deploy

# Native server (Docker or direct)
cargo build --release --features native --no-default-features --bin image-proxy-server
docker build -t image-proxy .
```

## Architecture

Cloudflare Worker written in Rust, compiled to WASM via `worker-build`.

**Request flow**: Origin check → parse params → SSRF check → domain allowlist → cache lookup → GET → validate → decode → resize → encode → cache store → respond.

```
src/
├── lib.rs         # Entry point: config + routing (cfg cloudflare)
├── native.rs      # Native axum/tokio server entry (cfg native, [[bin]])
├── config.rs      # Config struct from worker env vars (cfg cloudflare)
├── cors.rs        # Origin validation, CORS headers (cfg cloudflare)
├── handler.rs     # Worker request pipeline (cfg cloudflare)
├── security.rs    # Shared: domain allowlist, SSRF, content-type validation
├── params.rs      # Shared: query param parsing, cache key generation
├── process.rs     # Shared: JPEG→JPEG(lossy), PNG→WebP(lossless), GIF→passthrough
└── error.rs       # Shared: ProxyError enum → HTTP status codes
```

## Security Model

Three layers:

1. **Origin validation** (`cors.rs`) — `ALLOWED_ORIGINS` checks `Origin`/`Referer` headers. Requests without either are allowed through (cURL, same-origin) but layers 2+3 still protect.
2. **SSRF protection** (`security.rs`) — blocks private IPs (127.0.0.1, 10.x, 172.16-31.x, 192.168.x, 169.254.x, ::1), localhost, cloud metadata endpoints, and non-HTTP(S) schemes.
3. **Source domain allowlist** (`security.rs`) — `ALLOWED_DOMAINS` controls which image hosts can be proxied.

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `ALLOWED_ORIGINS` | `https://chartex.com,https://www.chartex.com` | Comma-separated origins allowed to call the proxy |
| `ALLOWED_DOMAINS` | *(empty = all)* | Comma-separated source image domains. **Set in production.** |
| `UPSTREAM_REFERER` | `https://chartex.com/` | Referer header sent to upstream CDNs |
| `MAX_WIDTH` | `4096` | Maximum output width in pixels |
| `MAX_HEIGHT` | `4096` | Maximum output height in pixels |
| `MAX_SIZE_MB` | `25` | Maximum source image size in MB |
| `CACHE_TTL` | `7776000` (90 days) | Cache-Control max-age in seconds |

## Output Format Strategy

| Input | Output | Rationale |
|---|---|---|
| JPEG | JPEG (lossy, quality=`q` param) | Lossy→lossy preserves size advantage, quality controllable |
| PNG | WebP lossless | Usually smaller than PNG, preserves transparency |
| GIF | Passthrough (no processing) | Preserves animation — decoding destroys frames |
| WebP (no resize) | Passthrough | Already optimal format |

## Caching

- Cloudflare Cache API (edge-cached per region)
- Cache key strips query params from source URL (only scheme + host + path)
- Cache key includes `q` (quality) and `fit` mode (stable Display strings, not Debug)
- Format: `https://image-proxy.internal/v1?url={path}&w={w}&h={h}&q={q}&fit={fit}`

## Crate Constraints

- **No FFI crates** — must compile to `wasm32-unknown-unknown` (no libc, no C deps)
- **No `tokio`** — the `worker` crate provides its own async runtime
- **No `rayon`** — no threading in WASM
- **No `webp` crate** — FFI to libwebp. Use `image` crate's `image-webp` encoder (lossless only)
- **No `ravif`** — AVIF encoding too slow without asm/threading
- **No `reqwest`** — use `worker::Fetch` for HTTP requests
- **No lossy WebP** — `image-webp` 0.2.x only supports lossless. JPEG inputs re-encode as JPEG with quality control instead.

## Memory Rules

- Drop source bytes immediately after decode (`drop(bytes)`)
- Use `into_raw()` for zero-copy buffer extraction in encoders
- Use `into_rgb8()` / `into_rgba8()` to consume DynamicImage (no extra copy)
- Pre-allocate output buffers (JPEG ~12% of raw, WebP ~50% of raw)

## Performance Notes

- **No HEAD request** — removed because many CDNs reject HEAD (TikTok, etc.) and it adds 50-200ms latency per cache miss. GET validates content-type and size on the actual body.
- **CatmullRom filter** — bicubic instead of Lanczos3. Visually indistinguishable for web images, ~2x faster.
- **Config parsed once** — `Config::from_env()` in `lib.rs`, passed by reference. No per-request env var lookups.

## Error Handling

All errors go through `ProxyError` enum → `into_response()`:
- Maps each variant to HTTP status (400/403/413/415/422/502)
- Returns plain text error message
- Error responses omit CORS headers

## Testing

```bash
npx wrangler dev

# Source URL MUST be percent-encoded
curl "http://localhost:8787/resize?url=$(python3 -c 'import urllib.parse; print(urllib.parse.quote("https://example.com/img.jpg?token=abc", safe=""))')&w=300&q=80"

# Test quality param
curl "http://localhost:8787/resize?url=...&w=300&q=60"  # Lower quality, smaller file
curl "http://localhost:8787/resize?url=...&w=300&q=95"  # Higher quality, larger file

# Test errors
curl "http://localhost:8787/resize"                    # 400 missing url
curl "http://localhost:8787/resize?url=http://x/a.svg" # 415 invalid content type
curl "http://localhost:8787/resize?url=http://127.0.0.1/x.jpg" # 403 SSRF blocked
```

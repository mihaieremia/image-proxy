//! Image Proxy — on-the-fly image resizing at the edge.
//!
//! Two build targets:
//! - `cloudflare` (default): Cloudflare Worker compiled to WASM
//! - `native`: Standalone tokio/axum server compiled to native binary

// --- Shared modules (used by both Worker and native builds) ---
pub mod error;
pub mod params;
pub mod process;
pub mod security;

// --- Cloudflare Worker modules ---
#[cfg(feature = "cloudflare")]
mod config;
#[cfg(feature = "cloudflare")]
mod cors;
#[cfg(feature = "cloudflare")]
mod handler;

// --- Cloudflare Worker entry point ---
#[cfg(feature = "cloudflare")]
use worker::*;

#[cfg(feature = "cloudflare")]
#[event(fetch, respond_with_errors)]
async fn main(req: Request, env: Env, ctx: Context) -> Result<Response> {
    let config = config::Config::from_env(&env);
    let allowed_origins = cors::allowed_origins(&env);

    if req.method() == Method::Options {
        return cors::handle_preflight(&req, &allowed_origins);
    }

    match req.path().as_str() {
        "/" | "/resize" => handler::handle_resize(req, ctx, &config, &allowed_origins).await,
        _ => Response::error("Not Found", 404),
    }
}

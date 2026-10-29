// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Sentin8:Web
// SPDX-License-Identifier: Apache-2.0

//! Sentin8 Web - single-instance static file server for all Flutter web apps.
//!
//! Web is one process that serves **all** Flutter web apps. Apps are
//! spread across multiple domain directories (`sentin8/apps/`, `bank8/apps/`,
//! `hotel8/apps/`, …). Web discovers each app's built web directory via
//! Valkey keys `captain:app_dir:{slug}` (registered by `vertical enroll`),
//! which contains the exact absolute path to the app's `build/web` directory.
//! This keeps sentin8 free of hardcoded vertical paths - vertical app
//! directories are registered at deployment time via the domain registry -
//! zero compile-time vertical knowledge.
//!
//! When a request arrives, `sentin8-tls` injects `X-Sentin8-Hostname` (the
//! SNI/Host) and `X-Sentin8-App-Id`. Web resolves the app slug from the
//! hostname (the first label before `.{env}.{org}.{domain}`) and serves
//! files from the app's `build/web` directory.
//!
//! Usage:
//!   web [--port <port>]
//!
//! Env vars (set by captain):
//!   SENTIN8_WEB_PORT - the port to bind
//!
//! Behaviour (ported from the per-app static server):
//!   - Serves files under {app_dir}/{slug}/build/web
//!   - SPA fallback: paths without a file extension → index.html
//!   - /v1/health endpoint for captain monitoring
//!   - Cross-origin isolation headers (COOP/COEP) for WASM threads
//!   - Strips service-worker registration from flutter_bootstrap.js
//!   - Path traversal protection

mod policy;
mod registration;

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Self-registration probe: `--print-registration`
    // prints the web host's self-description (m2m_grants) as JSON and exits.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--print-registration") {
        let section = args
            .iter()
            .find_map(|a| a.strip_prefix("--section="))
            .map(|s| s.to_string());
        return registration::print(section);
    }

    // Install the Graviola PQ crypto provider before any reqwest HTTPS call.
    let _ = sentin8::tls::crypto::sentin8_pq_provider().install_default();

    let _ = dotenvy::dotenv();

    // Shield policy: boot load refuses to start on invalid
    // policy text; hot-reload; shared deps; stat emitter.
    //
    // DB identity (VDC §VDC.3b): the boot pool connects under web's OWN
    // per-service DSN (`SENTIN8_WEB_DATABASE_URL` → `sentin8_web_service`,
    // bootstrap-tier SCRAM). The shared dev DSN (`sentin8_app`) applies
    // only when the per-service var is unset - local test harnesses; the
    // deployed posture is the dedicated role.
    {
        let dsn = std::env::var("SENTIN8_WEB_DATABASE_URL")
            .unwrap_or_else(|_| sentin8::config::Sentin8Config::default_database_url().to_string());
        let pool = sentin8::config::pg_pool::connect(&dsn)
            .await
            .map_err(|e| anyhow::anyhow!("policy boot pool: {e}"))?;
        init_web_policy(&pool).await?;
    }

    let _ = sentin8::middleware::error_metrics::init_tracing_with_metrics(
        "sentin8:web",
        std::env::var("RUST_LOG").ok().as_deref().unwrap_or("info"),
    );
    sentin8::middleware::error_metrics::spawn_publisher(
        std::env::var("VALKEY_URL").unwrap_or_default(),
        "sentin8:web",
    );

    // Parse args: [--web-root <dir>] [--port <port>]
    //
    // `--web-root` is used by dash (the platform admin UI) which is a
    // single-app server - not part of the app-store multi-app discovery.
    // When `--web-root` is given, web serves that single directory for
    // all requests (legacy per-app mode, same as the old static-server).
    let mut web_root_override: Option<String> = std::env::var("SENTIN8_WEB_ROOT")
        .ok()
        .filter(|s| !s.is_empty());
    let mut port: u16 = std::env::var("SENTIN8_WEB_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--web-root" | "-w" => {
                if let Some(v) = args.next() {
                    web_root_override = Some(v);
                }
            }
            "--port" | "-p" => {
                if let Some(v) = args.next() {
                    port = v.parse().unwrap_or(port);
                }
            }
            _ => {}
        }
    }

    let addr = format!("0.0.0.0:{port}");

    // Build the TLS instance (Acme certs, on-demand SNI resolution, mapping
    // cache, context middleware - same as any Rust service). The resolver
    // is database-free: cache miss → Sys owner API .
    let tls = sentin8::tls::Sentin8Tls::builder()
        .service_identity("sentin8:web")
        .build()
        .await?;

    // Traffic counter - counts every inbound request (excludes health checks).
    let counter = sentin8::middleware::RequestCounter::new();
    sentin8::middleware::spawn_flush_loop(
        counter.clone(),
        std::env::var("VALKEY_URL")
            .unwrap_or_else(|_| sentin8::config::Sentin8Config::default_valkey_url().to_string()),
        10,
    );

    // ── Single-app mode (dash) ──────────────────────────────────────
    // When --web-root is given, serve that single directory for all
    // requests. This is the legacy mode used by dash (platform admin UI).
    if let Some(root) = web_root_override {
        let web_root = PathBuf::from(&root)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(&root));
        if !web_root.join("index.html").is_file() {
            anyhow::bail!(
                "index.html not found in {} - run `flutter build web` first",
                web_root.display()
            );
        }
        tracing::info!(
            service = "sentin8:web",
            version = %sentin8::build_info::id(),
            mode = "single-app",
            web_root = %web_root.display(),
            port,
            "service starting"
        );
        let state = SingleAppState { web_root };
        let app = build_single_app_router(state, counter.clone());
        let shutdown_name: &'static str = "sentin8:web";
        return tls.serve(&addr, app, shutdown_name).await;
    }

    // ── Multi-app mode ───────────────────────────────────────────────
    // App discovery is via Valkey `captain:app_dir:{slug}` keys (registered
    // by `sentin8-cli vertical enroll`). Each key contains the absolute
    // path to the app's built web directory. No filesystem scanning -
    // Valkey is the single source of truth for app locations.
    tracing::info!(
        service = "sentin8:web",
        version = %sentin8::build_info::id(),
        mode = "multi-app",
        port,
        "service starting"
    );

    // Build the static-file router.
    let valkey_url = std::env::var("VALKEY_URL")
        .unwrap_or_else(|_| sentin8::config::Sentin8Config::default_valkey_url().to_string());
    let valkey = if !valkey_url.is_empty() {
        // Mint-or-load the web workload's Valkey-plane pair in a
        // background loop (the mint races Acme's boot on a fresh
        // platform); once the pair lands, the app-dir client connects on
        // the next construction - group DESIGN.md §3.1.
        {
            let issuer = sentin8::config::Sentin8Config::from_env().sentin8_auth_issuer();
            let ca_cert_path = sentin8::auth::m2m::platform_ca_path();
            tokio::spawn(async move {
                loop {
                    match sentin8::auth::m2m::ensure_workload_certificate(
                        &issuer,
                        "sentin8:web",
                        &ca_cert_path,
                    )
                    .await
                    {
                        Ok(_) => break,
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "web: Valkey workload identity mint pending - retrying in 10s"
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                        }
                    }
                }
            });
        }
        sentin8::middleware::valkey_client(&valkey_url, "sentin8:web")
            .await
            .ok()
    } else {
        None
    };
    let state = MultiAppState { valkey };
    let app = build_multi_app_router(state, counter, &tls);
    let shutdown_name: &'static str = "sentin8:web";

    // Use sentin8-tls serve() which handles TLS termination + graceful shutdown.
    tls.serve(&addr, app, shutdown_name).await
}

/// Build the axum router for multi-app static file serving (app-store mode).
fn build_multi_app_router(
    state: MultiAppState,
    counter: sentin8::middleware::RequestCounter,
    tls: &sentin8::tls::Sentin8Tls,
) -> Router {
    Router::new()
        .route("/v1/health", get(health_handler))
        // Proxy /v1/app/* to the sentin8-app service so Flutter apps can call
        // the runtime context endpoint on their own origin (the TLS proxy
        // injects X-Sentin8-Hostname via context middleware).
        .route("/v1/app/runtime/context", get(proxy_to_app_service))
        .fallback(any(spa_or_static_multi_app_handler).with_state(state))
        .layer(axum::middleware::from_fn_with_state(
            counter,
            sentin8::middleware::traffic_counter_layer,
        ))
        .layer(axum::middleware::from_fn_with_state(
            sentin8::tls::ContextMiddlewareState {
                resolver: tls.host_context_resolver(),
            },
            sentin8::tls::context_middleware,
        ))
}

/// Build the axum router for single-app static file serving (dash mode).
fn build_single_app_router(
    state: SingleAppState,
    counter: sentin8::middleware::RequestCounter,
) -> Router {
    Router::new()
        .route("/v1/health", get(health_handler))
        .fallback(any(spa_or_static_single_app_handler).with_state(state))
        .layer(axum::middleware::from_fn_with_state(
            counter,
            sentin8::middleware::traffic_counter_layer,
        ))
}

/// Shared multi-app state. `pub` for the integration tests
/// (`tests/discovery.rs`, web DESIGN §9) which build a test router over
/// the real handlers with a test-local Valkey connection.
#[derive(Clone)]
pub struct MultiAppState {
    /// Valkey connection manager for looking up `captain:app_dir:{slug}` keys.
    /// Registered by `vertical enroll` - contains the absolute path to each
    /// app's built web directory.
    pub valkey: Option<glide::GlideClient>,
}

#[derive(Clone)]
struct SingleAppState {
    /// The single web root directory (dash mode).
    web_root: PathBuf,
}

/// Proxy /v1/app/runtime/context to the sentin8-app service.
/// The context middleware has already injected X-Sentin8-* headers,
/// so the app service can resolve the app instance from the hostname.
///
/// PUBLIC, pre-login surface: this is the bootstrap
/// call every app makes before any user signs in - the Flutter SDK
/// holds no token yet, and the caller identity is the app instance
/// established by the TLS-injected `X-Sentin8-App-Id` headers, not a
/// bearer principal. A bearer-token + Shield
/// gate here would 401 every app bootstrap - the gate stays off.
/// The response carries only instance metadata (auth domain, branding,
/// feature flags) - same trust basis as the static app shell.
///
/// `pub` for the integration tests (`tests/discovery.rs`, web DESIGN §9)
/// which wire this handler into a test router.
pub async fn proxy_to_app_service(headers: HeaderMap) -> Response {
    let config = sentin8::config::Sentin8Config::from_env();
    let app_url = format!("{}/v1/app/runtime/context", config.sentin8_app_url());
    let client = reqwest::Client::builder().build().unwrap_or_default();
    let mut forwarded = reqwest::header::HeaderMap::new();
    for (key, value) in headers.iter() {
        if key.as_str().starts_with("x-sentin8-") || key == "host" {
            if let Ok(v) = value.to_str() {
                if let Ok(hv) = reqwest::header::HeaderValue::from_str(v) {
                    forwarded.insert(key.clone(), hv);
                }
            }
        }
    }
    match client.get(app_url).headers(forwarded).send().await {
        Ok(resp) => {
            let status = axum::http::StatusCode::from_u16(resp.status().as_u16())
                .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json")
                .to_string();
            let body = resp.bytes().await.unwrap_or_default();
            Response::builder()
                .status(status)
                .header("content-type", content_type)
                .body(axum::body::Body::from(body))
                .unwrap()
        }
        Err(e) => {
            tracing::warn!(error = %e, "web: failed to proxy to app service");
            Response::builder()
                .status(axum::http::StatusCode::BAD_GATEWAY)
                .body(axum::body::Body::from(format!(
                    "App service unavailable: {e}"
                )))
                .unwrap()
        }
    }
}

async fn health_handler() -> impl IntoResponse {
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "status": "ok",
            "service": "sentin8:web",
            // version = the joined identity (+git hash, .d on
            // debug), build = the raw hash (captain's collapse rule).
            "version": sentin8::build_info::id(),
            "build": sentin8::build_info::build_hash(),
        })),
    )
}

/// Multi-app fallback handler: resolve the app slug from the
/// `X-Sentin8-Hostname` header (injected by sentin8-tls), then serve the file
/// from that app's web root. SPA fallback for paths without a file extension.
///
/// `pub` for the integration tests (`tests/discovery.rs`, web DESIGN §9).
pub async fn spa_or_static_multi_app_handler(
    State(state): State<MultiAppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    // Resolve the app slug from the request hostname.
    //
    // Prefer `X-Sentin8-Hostname` when an L7 layer (sentin8-tls context
    // middleware) injects it. Proxy, however, is L4 (byte-piping) and does
    // NOT inject that header - so fall back to the HTTP `Host` header (port
    // stripped), which carries the original app domain (e.g.
    // `ebank.prod.ibank.local`). Without this fallback the slug is always
    // empty under proxy and every multi-app hostname 404s with
    // "no app found for hostname \"\"".
    let hostname = headers
        .get("x-sentin8-hostname")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            headers
                .get("host")
                .and_then(|v| v.to_str().ok())
                .map(|h| h.split(':').next().unwrap_or(h))
        })
        .unwrap_or("");

    let web_root = resolve_web_root(&state, hostname).await;
    let Some(web_root) = web_root else {
        return not_found(&format!(
            "no app found for hostname \"{hostname}\" (no captain:app_dir key for slug)"
        ));
    };

    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    serve_from_web_root(&web_root, &uri, if_none_match).await
}

/// Single-app fallback handler (dash): serve from the fixed web root.
async fn spa_or_static_single_app_handler(
    State(state): State<SingleAppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    serve_from_web_root(&state.web_root, &uri, if_none_match).await
}

/// Shared file-serving logic for both single-app and multi-app modes.
/// `if_none_match` - the caller's `If-None-Match` header value :
/// a match against the file's ETag short-circuits to 304 without a body.
async fn serve_from_web_root(web_root: &Path, uri: &Uri, if_none_match: Option<&str>) -> Response {
    let path = uri.path();
    let decoded = percent_encoding::percent_decode_str(path).decode_utf8_lossy();

    // Path traversal protection: resolve under web_root and check.
    let requested = web_root.join(decoded.trim_start_matches('/'));
    let canonical = requested.canonicalize().unwrap_or(requested.clone());
    if !canonical.starts_with(web_root) {
        return not_found(path);
    }

    // If the file exists, serve it (with special handling for bootstrap).
    if canonical.is_file() {
        return serve_file(&canonical, if_none_match).await;
    }

    // SPA fallback: no file extension → serve index.html for client-side routing.
    if !decoded.contains('.') {
        let index_html = web_root.join("index.html");
        if index_html.is_file() {
            return serve_file(&index_html, if_none_match).await;
        }
    }

    not_found(path)
}

/// Resolve the web root for a given hostname.
///
/// The hostname is the app's domain - e.g. `ebank.prod.ibank.local`.
/// The first label (`ebank`) is the app slug.
///
/// Resolution: Valkey `captain:app_dir:{slug}` key (registered by
/// `vertical enroll`) - contains the absolute path to the built web directory.
///
/// Discovery is on-the-fly - apps built or registered after startup are
/// served without a restart.
async fn resolve_web_root(state: &MultiAppState, hostname: &str) -> Option<PathBuf> {
    if hostname.is_empty() {
        return None;
    }

    // Extract the first label (the app slug) from the hostname.
    // e.g. "ebank.prod.ibank.local" → "ebank"
    let slug = hostname.split('.').next().unwrap_or("");

    if slug.is_empty() {
        return None;
    }

    // Check Valkey for the app directory mapping (GLIDE typed GET).
    if let Some(conn) = &state.valkey {
        let key = format!("captain:app_dir:{slug}");
        use glide::AsyncCommands as _;
        if let Ok(Some(path_str)) = conn.get::<_, Option<String>>(&key).await {
            let web_root = PathBuf::from(&path_str);
            if web_root.join("index.html").is_file() {
                return Some(web_root);
            }
        }
    }

    None
}

/// Serve a single file with appropriate headers. For flutter_bootstrap.js,
/// strip service-worker registration in-memory before serving.
///
/// Caching (/, web DESIGN §10):
/// - `flutter_bootstrap.js` (runtime settings are baked in per request) and
///   `index.html` (references fingerprinted assets) stay `no-store`.
/// - Fingerprinted assets (`main.dart.js`, `*.wasm`, fonts, other static
///   files) get `Cache-Control: public, max-age=31536000, immutable` plus a
///   strong ETag (`"<size>-<mtime>"`); a matching `If-None-Match` returns
///   304 without a body.
async fn serve_file(path: &Path, if_none_match: Option<&str>) -> Response {
    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

    let is_bootstrap = filename == "flutter_bootstrap.js";
    let is_index = filename == "index.html";
    let is_wasm_thread = is_bootstrap
        || filename == "main.dart.js"
        || path.extension().and_then(|e| e.to_str()) == Some("wasm");

    // Read the file. For bootstrap, we modify it in-memory.
    if is_bootstrap {
        match tokio::fs::read_to_string(path).await {
            Ok(body) => {
                let patched = strip_service_worker_settings(&body);
                let bytes = patched.into_bytes();
                let mut resp = (
                    StatusCode::OK,
                    [(
                        header::CONTENT_TYPE,
                        "application/javascript; charset=utf-8",
                    )],
                    [(header::CACHE_CONTROL, "no-store, must-revalidate")],
                    bytes,
                )
                    .into_response();
                if is_wasm_thread {
                    let h = resp.headers_mut();
                    h.insert("cross-origin-opener-policy", "same-origin".parse().unwrap());
                    h.insert(
                        "cross-origin-embedder-policy",
                        "require-corp".parse().unwrap(),
                    );
                }
                return resp;
            }
            Err(_) => return not_found(path.to_str().unwrap_or("")),
        }
    }

    // For non-bootstrap files, use tower-http ServeDir-style streaming.
    match tokio::fs::metadata(path).await {
        Ok(meta) => {
            let mime = mime_guess::from_path(path)
                .first_or_octet_stream()
                .essence_str()
                .to_string();
            // strong validator from size + mtime (no content read -
            // the body streams from disk). Quoted per RFC 7232.
            let etag = format!(
                "\"{:x}-{:x}\"",
                meta.len(),
                meta.modified()
                    .ok()
                    .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            );
            // `If-None-Match: *` or an exact tag match → 304. The
            // header may carry a comma-separated list (`"a", "b"`) or `*`.
            if let Some(inm) = if_none_match {
                let matches = inm
                    .split(',')
                    .map(str::trim)
                    .any(|tag| tag == etag || tag == "*");
                if matches {
                    let mut resp = StatusCode::NOT_MODIFIED.into_response();
                    resp.headers_mut().insert(
                        header::ETAG,
                        header::HeaderValue::from_str(&etag)
                            .unwrap_or(header::HeaderValue::from_static("\"\"")),
                    );
                    return resp;
                }
            }
            let file = match tokio::fs::File::open(path).await {
                Ok(f) => f,
                Err(_) => return not_found(path.to_str().unwrap_or("")),
            };
            let stream = tokio_util::io::ReaderStream::new(file);
            let body = axum::body::Body::from_stream(stream);

            let mut resp = (
                StatusCode::OK,
                [(header::CONTENT_TYPE, mime.as_str())],
                [(header::CONTENT_LENGTH, meta.len().to_string())],
                body,
            )
                .into_response();
            {
                let h = resp.headers_mut();
                h.insert(
                    header::ETAG,
                    header::HeaderValue::from_str(&etag)
                        .unwrap_or(header::HeaderValue::from_static("\"\"")),
                );
                // index.html must revalidate every load (it
                // references fingerprinted assets); everything else is
                // fingerprinted/immutable content → cache hard.
                if is_index {
                    h.insert(
                        header::CACHE_CONTROL,
                        header::HeaderValue::from_static("no-store, must-revalidate"),
                    );
                } else {
                    h.insert(
                        header::CACHE_CONTROL,
                        header::HeaderValue::from_static("public, max-age=31536000, immutable"),
                    );
                }
            }
            if is_wasm_thread {
                let h = resp.headers_mut();
                h.insert("cross-origin-opener-policy", "same-origin".parse().unwrap());
                h.insert(
                    "cross-origin-embedder-policy",
                    "require-corp".parse().unwrap(),
                );
            }
            resp
        }
        Err(_) => not_found(path.to_str().unwrap_or("")),
    }
}

fn not_found(path: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({"error": "not found", "path": path})),
    )
        .into_response()
}

/// Strip Flutter's service worker registration from the bootstrap script.
fn strip_service_worker_settings(body: &str) -> String {
    let needle = "serviceWorkerSettings:";
    if let Some(start) = body.find(needle) {
        let after_key = &body[start + needle.len()..];
        if let Some(brace_start) = after_key.find('{') {
            let mut depth = 0i32;
            let mut end = brace_start;
            for (i, c) in after_key[brace_start..].char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = brace_start + i + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let abs_start = start + needle.len() + brace_start;
            let abs_end = start + needle.len() + end;
            let mut result = String::with_capacity(body.len());
            result.push_str(&body[..abs_start]);
            result.push_str("{}");
            result.push_str(&body[abs_end..]);
            return result;
        }
    }
    body.to_string()
}

// ── Unit tests ──
//
// Pure seams only: service-worker stripping, path-traversal protection,
// and SPA fallback. These run under `make test-rust` with no backing
// services; the TLS/router wiring needs a live stack and is covered by
// the e2e suite.

// ── Shield policy ──────────────────────────────────────────────────────
//
// The policy engine still boots + hot-reloads (uniform enforced posture,
// enforced posture) - web simply has NO token-bearing handler left to gate:
// the runtime-context proxy is public pre-login,
// static assets are the app shell. WEB_AUTH was removed with the
// runtime-proxy bearer gate - no verify_bearer_token call sites remain.

static WEB_POLICY: std::sync::OnceLock<std::sync::Arc<policy::Engine>> = std::sync::OnceLock::new();

/// Boot the policy engine via the ONE shared boot helper (web is a
/// custom-boot binary without the shell). Refuse-to-start on invalid text
/// (refuse-to-start on invalid policy text).
async fn init_web_policy(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    let engine = policy::engine();
    // Already inside the tokio runtime (`#[tokio::main]`) - awaited directly
    // by the caller. A nested `Handle::block_on` panics "Cannot start a
    // runtime from within a runtime" - the caller awaits it directly.
    sentin8::shield::runtime::boot_full(
        pool,
        &sentin8::config::Sentin8Config::from_env().sentin8_auth_url(),
        "sentin8:web",
        &engine,
    )
    .await?;
    let _ = WEB_POLICY.set(engine);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    // ── strip_service_worker_settings ──────────────────────────────────

    #[test]
    fn strip_replaces_service_worker_object_with_empty_object() {
        let body = r#"const config = {app: "x", serviceWorkerSettings: {"enabled": true, "scope": "/"}, other: 1};"#;
        let out = strip_service_worker_settings(body);
        assert!(out.contains("serviceWorkerSettings: {}"), "got: {out}");
        assert!(!out.contains("\"enabled\""), "inner settings must be gone");
        assert!(out.contains("other: 1"), "siblings must be preserved");
    }

    #[test]
    fn strip_handles_nested_braces() {
        let body = r#"serviceWorkerSettings: {"a": {"b": 1}, "c": "}" }"#;
        let out = strip_service_worker_settings(body);
        // The brace-depth counter must survive a stringified '}' inside.
        assert!(out.contains("serviceWorkerSettings: {}"), "got: {out}");
        assert!(!out.contains("\"b\""), "nested object must be stripped");
    }

    #[test]
    fn strip_is_noop_without_settings() {
        let body = "const x = 1; bootstrap();
";
        assert_eq!(strip_service_worker_settings(body), body);
    }

    #[test]
    fn strip_is_noop_without_object_open_brace() {
        let body = "serviceWorkerSettings: undefined;";
        assert_eq!(strip_service_worker_settings(body), body);
    }

    // ── serve_from_web_root: traversal protection + SPA fallback ───────

    fn tmp_web_root() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sentin8-web-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("index.html"), "<html>index</html>").unwrap();
        std::fs::write(dir.join("assets/app.js"), "console.log(1);").unwrap();
        std::fs::write(
            dir.join("flutter_bootstrap.js"),
            r#"serviceWorkerSettings: {"enabled": true}"#,
        )
        .unwrap();
        dir
    }

    async fn body_string(resp: Response) -> String {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        String::from_utf8_lossy(&bytes).to_string()
    }

    #[tokio::test]
    async fn serves_index_at_root() {
        let root = tmp_web_root();
        let resp = serve_from_web_root(&root, &axum::http::Uri::from_static("/"), None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        assert!(body.contains("index"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn serves_static_asset_with_mime() {
        let root = tmp_web_root();
        let resp =
            serve_from_web_root(&root, &axum::http::Uri::from_static("/assets/app.js"), None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(ct.contains("javascript"), "content-type: {ct}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn spa_fallback_for_extensionless_path() {
        let root = tmp_web_root();
        let resp = serve_from_web_root(
            &root,
            &axum::http::Uri::from_static("/some/client/route"),
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(body_string(resp).await.contains("index"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn missing_file_with_extension_404s() {
        let root = tmp_web_root();
        let resp = serve_from_web_root(
            &root,
            &axum::http::Uri::from_static("/no/such/file.png"),
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn path_traversal_is_blocked() {
        let root = tmp_web_root();
        let outside = std::env::temp_dir().join(format!(
            "sentin8-web-secret-{}.txt",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&outside, "secret").unwrap();

        // Canonicalize-based escape attempts must 404, never serve the file.
        for path in [
            "/../../etc/passwd",
            "/..%2F..%2Fetc%2Fpasswd",
            format!("/../../{}", outside.file_name().unwrap().to_str().unwrap()).as_str(),
        ] {
            let resp = serve_from_web_root(
                &root,
                &axum::http::Uri::builder()
                    .path_and_query(path)
                    .build()
                    .unwrap(),
                None,
            )
            .await;
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "traversal path must be blocked: {path}"
            );
        }
        std::fs::remove_file(&outside).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn bootstrap_is_patched_in_memory() {
        let root = tmp_web_root();
        let resp = serve_from_web_root(
            &root,
            &axum::http::Uri::from_static("/flutter_bootstrap.js"),
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        // Read headers BEFORE consuming the body (response is moved by
        // body_string).
        let coop = resp
            .headers()
            .get("cross-origin-opener-policy")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let coep = resp
            .headers()
            .get("cross-origin-embedder-policy")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = body_string(resp).await;
        assert!(body.contains("serviceWorkerSettings: {}"), "got: {body}");
        assert!(!body.contains("\"enabled\""), "settings must be stripped");
        // COOP/COEP headers are required for WASM threads.
        assert_eq!(coop, "same-origin");
        assert_eq!(coep, "require-corp");
        std::fs::remove_dir_all(&root).unwrap();
    }

    // ── Hostname → slug resolution (multi-app mode) ────────────────────

    #[test]
    fn slug_is_first_hostname_label() {
        // Mirrors the split('.') logic in resolve_web_root; the function
        // itself needs Valkey, the rule is pinned here.
        let hostname = "ebank.prod.ibank.local";
        assert_eq!(hostname.split('.').next().unwrap_or(""), "ebank");
        assert_eq!("".split('.').next().unwrap_or(""), "");
    }
}

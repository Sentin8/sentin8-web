// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Sentin8:Web
// SPDX-License-Identifier: Apache-2.0

//! Multi-app discovery + runtime-context proxy integration tests
//! (web DESIGN §9 — testing-strategy §3 runtime-skip).
//!
//! Covered:
//! * `discovery_serves_registered_app` — the full multi-app fallback
//!   handler (`spa_or_static_multi_app_handler`) resolves the app slug
//!   from the HTTP `Host` header, reads `captain:app_dir:{slug}` from the
//!   real Valkey, and serves the app's `index.html` (200).
//! * `discovery_unknown_hostname_404` — a hostname whose slug has no
//!   `captain:app_dir` key is a 404 (never a wrong app's content).
//! * `proxy_forwards_context_headers` — the runtime-context proxy relays
//!   `x-sentin8-*` + `host` headers to the app service and returns the
//!   instance-metadata JSON. Opt-in (needs `SENTIN8_APP_URL`); skipped
//!   otherwise, thin by design.
//!
//! Runtime-skip gating: Valkey and PostgreSQL are probed at the bottom of
//! each fixture with hard connect timeouts; when unreachable the test
//! prints one skip line and passes (the `db_`-style convention of
//! storage/cron tests). The discovery tests need ONLY Valkey — PostgreSQL
//! is probed once per process so the whole target skips together on an
//! infra-less host. The proxy test is opt-in via `SENTIN8_APP_URL`.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

// The web host is a binary-only crate — include the main module the way
// the cron test crate includes `ticker.rs` (no lib target to import).
// Its inner `mod policy;` / `mod registration;` resolve to the real
// `src/` files; dead_code is allowed because only the discovery/proxy
// seams are exercised by these tests.
#[allow(dead_code)]
#[path = "../src/main.rs"]
mod web;

use web::MultiAppState;

// ── Fixtures ────────────────────────────────────────────────────────────

/// Install the Graviola PQ crypto provider once — reqwest panics with
/// "No provider set" when the proxy handler builds its client without one.
fn install_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = sentin8::tls::crypto::sentin8_pq_provider().install_default();
    });
}

/// A unique app slug per test — the `captain:app_dir:{slug}` key this
/// test writes is deleted in teardown, but a random slug additionally
/// guarantees isolation from parallel tests and from real enrolled apps.
fn mk_slug() -> String {
    format!("it-app-{}", uuid::Uuid::new_v4().simple())
}

/// A tempdir web root containing `index.html` — the shape the discovery
/// path accepts (root + index.html present).
fn app_root_with_index(marker: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    std::fs::write(
        root.join("index.html"),
        format!("<html>sentin8-discovery-test {marker}</html>"),
    )
    .expect("write index.html");
    (tmp, root)
}

/// Connect to the platform Valkey (runtime-skip when unreachable).
///
/// The probe is bounded by a hard connect timeout: `ConnectionManager::new`
/// retries refused connects internally (backon: 6 retries, up to a 60s cap
/// per delay) and would otherwise stall the skip path for minutes on an
/// infra-less host.
async fn valkey_conn() -> Option<glide::GlideClient> {
    install_provider();
    let _ = dotenvy::dotenv();
    let url = std::env::var("VALKEY_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| sentin8::config::Sentin8Config::default_valkey_url().to_string());
    match tokio::time::timeout(
        std::time::Duration::from_secs(3),
        sentin8::middleware::valkey_client(&url, "sentin8:web"),
    )
    .await
    {
        Ok(Ok(c)) => Some(c),
        Ok(Err(e)) => {
            eprintln!("skipping web discovery tests — Valkey unreachable: {e}");
            None
        }
        Err(_) => {
            eprintln!("skipping web discovery tests — Valkey connect timed out");
            None
        }
    }
}

/// PostgreSQL probe — printed once per process so the target skips as a
/// whole when the infra is down (the discovery tests themselves never
/// touch the DB; the policy boot is not exercised in tests). Bounded by a
/// connect timeout so a blackholed host cannot stall the skip path.
async fn postgres_reachable() -> bool {
    let connect =
        sqlx::postgres::PgPool::connect(sentin8::config::Sentin8Config::default_database_url());
    match tokio::time::timeout(std::time::Duration::from_secs(5), connect).await {
        Ok(Ok(p)) => {
            p.close().await;
            true
        }
        Ok(Err(e)) => {
            eprintln!("skipping web discovery tests — PostgreSQL unreachable: {e}");
            false
        }
        Err(_) => {
            eprintln!("skipping web discovery tests — PostgreSQL connect timed out");
            false
        }
    }
}

/// Build the multi-app router over the real handlers with a test-local
/// Valkey connection. Mirrors `build_multi_app_router` minus the TLS
/// context middleware (the tests drive the handler through the same
/// public surface, passing `Host` — the L4-proxy fallback the handler
/// supports by contract).
fn test_router(state: MultiAppState) -> axum::Router {
    axum::Router::new()
        .route(
            "/v1/app/runtime/context",
            axum::routing::get(web::proxy_to_app_service),
        )
        .fallback(axum::routing::any(web::spa_or_static_multi_app_handler))
        .with_state(state)
}

/// SET captain:app_dir:{slug} → returns the key for teardown.
async fn register_app(conn: &glide::GlideClient, slug: &str, root: &Path) -> String {
    use glide::AsyncCommands as _;
    let key = format!("captain:app_dir:{slug}");
    let path_str = root.to_string_lossy().to_string();
    conn.set::<_, _, ()>(&key, path_str)
        .await
        .expect("SET captain:app_dir");
    key
}

/// Delete the test key — teardown contract: no production keys left behind.
async fn delete_key(conn: &glide::GlideClient, key: &str) {
    use glide::AsyncCommands as _;
    let _: Result<i64, _> = conn.del(key).await;
}

// ── Multi-app discovery ──────────────────────────────────────────────

#[tokio::test]
async fn discovery_serves_registered_app() {
    let Some(conn) = valkey_conn().await else {
        return;
    };
    if !postgres_reachable().await {
        return;
    }
    let slug = mk_slug();
    let (_tmp, root) = app_root_with_index(&slug);
    let key = register_app(&conn, &slug, &root).await;

    // The fallback handler must be exercised with its state attached —
    // the router-level `.fallback(handler).with_state(state)` shape is
    // the same wiring `build_multi_app_router` uses.
    let state = MultiAppState {
        valkey: Some(conn.clone()),
    };
    let app = test_router(state);

    // Host header {slug}.test → slug {slug} → key hit → index.html 200.
    let resp = app
        .oneshot(
            Request::builder()
                .header("host", format!("{slug}.test"))
                .uri("/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router oneshot");
    assert_eq!(resp.status(), StatusCode::OK, "registered app must serve");
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&bytes).to_string();
    assert!(
        body.contains(&slug),
        "served body must be the fixture index.html (marker {slug}), got: {body}"
    );

    delete_key(&conn, &key).await;
}

#[tokio::test]
async fn discovery_unknown_hostname_404s() {
    let Some(conn) = valkey_conn().await else {
        return;
    };
    if !postgres_reachable().await {
        return;
    }
    // An unregistered slug must 404 — never a wrong app's content.
    let ghost = format!("no-such-app-{}", uuid::Uuid::new_v4().simple());
    let state = MultiAppState {
        valkey: Some(conn.clone()),
    };
    let app = test_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .header("host", format!("{ghost}.test"))
                .uri("/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router oneshot");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&bytes).to_string();
    assert!(
        body.contains("no app found for hostname"),
        "404 must name the unresolvable hostname, got: {body}"
    );
}

/// The L4-proxy Host fallback (web DESIGN §2): under proxy no
/// `X-Sentin8-Hostname` header is injected, so the handler must resolve
/// the slug from the HTTP `Host` header (port stripped).
#[tokio::test]
async fn discovery_resolves_slug_from_host_header_under_proxy() {
    let Some(conn) = valkey_conn().await else {
        return;
    };
    if !postgres_reachable().await {
        return;
    }
    let slug = mk_slug();
    let (_tmp, root) = app_root_with_index(&slug);
    let key = register_app(&conn, &slug, &root).await;

    let state = MultiAppState {
        valkey: Some(conn.clone()),
    };
    let app = test_router(state);
    // Host with an explicit port — the port must be stripped before the
    // slug split (the production proxy case).
    let resp = app
        .oneshot(
            Request::builder()
                .header("host", format!("{slug}.test:8443"))
                .uri("/some/client/route")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router oneshot");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "SPA fallback via Host header (port stripped) must serve index.html"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains(&slug));

    delete_key(&conn, &key).await;
}

// ── Runtime-context proxy ─────────────────────────────────────────────

/// Opt-in: the proxy's upstream is the app service resolved from
/// `Sentin8Config` (no `SENTIN8_APP_URL` override exists in production
/// code), so the test only runs when the operator exports `SENTIN8_APP_URL`
/// pointing at a reachable app service — the domain it carries re-aims the
/// handler's derived upstream. The probed hostname defaults to
/// `dash.ops.sys.sentin8.local` (the platform's own app instance — present
/// on every live stack); override with `SENTIN8_APP_TEST_HOSTNAME`. The
/// response must echo instance metadata (the proxy forwards `x-sentin8-*`
/// + `host` headers only).
#[tokio::test]
async fn proxy_forwards_context_headers() {
    install_provider();
    let Some(app_base) = std::env::var("SENTIN8_APP_URL")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        eprintln!(
            "skipping proxy_forwards_context_headers — SENTIN8_APP_URL not set (set it to a reachable sentin8:app base URL to run)"
        );
        return;
    };

    // The handler builds its upstream from Sentin8Config; point the domain
    // at the provided base so the request lands on the reachable app.
    let host = app_base
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("")
        .to_string();
    std::env::set_var("SENTIN8_DOMAIN", domain_from_host(&host));

    let hostname = std::env::var("SENTIN8_APP_TEST_HOSTNAME")
        .unwrap_or_else(|_| "dash.ops.sys.sentin8.local".to_string());
    let headers: axum::http::HeaderMap = [
        ("x-sentin8-hostname", hostname.as_str()),
        ("host", hostname.as_str()),
    ]
    .into_iter()
    .filter_map(|(k, v)| {
        Some((
            axum::http::HeaderName::from_bytes(k.as_bytes()).ok()?,
            axum::http::HeaderValue::from_str(v).ok()?,
        ))
    })
    .collect();
    let resp = web::proxy_to_app_service(headers).await;
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&bytes).to_string();
    assert_eq!(
        status,
        StatusCode::OK,
        "app runtime context must resolve for {hostname} — body: {body}"
    );
    let ctx: serde_json::Value =
        serde_json::from_str(&body).expect("runtime context must be a JSON object");
    for field in ["org_id", "app_slug", "hostname", "sentin8_bff_domain"] {
        assert!(
            ctx.get(field).is_some(),
            "instance metadata missing contract field {field}: {body}"
        );
    }
}

/// The org-domain portion of `app.ops.sys.{domain}` — the domain segment
/// the proxy's `Sentin8Config` needs to rebuild the app URL.
fn domain_from_host(host: &str) -> String {
    // host = "app.ops.sys.sentin8.local" → strip the three service labels.
    host.split('.').skip(3).collect::<Vec<_>>().join(".")
}

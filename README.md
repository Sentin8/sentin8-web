# Design Doc: Sentin8:Web — Flutter Web Static Host

## 1. Summary

Sentin8:Web is the single-instance static file server for every Flutter
web app in the platform. One process serves all apps across all verticals:
app locations are discovered at request time from runtime-registered
Valkey keys (`captain:app_dir:{slug}`, written by `vertical enroll`), so
the host holds zero compile-time knowledge of any app or vertical. Every
request is PQ-TLS-terminated in-process (Sentin8:TLS), the hostname picks
the app, static assets stream from the app's built `build/web` directory
with an SPA fallback, and the `/v1/app/runtime/context` bootstrap call is
proxied to Sentin8:App on the same origin.

## 2. Motivation

- **One process per machine for all app shells.** The platform can host
  dozens of app instances (dash, vertical admin apps, tenant apps). A
  per-app static server would multiply processes, certificates, and
  monitoring; a single multi-app host with runtime discovery keeps the
  fleet small and apps deployable without restarts.
- **Zero compile-time vertical knowledge.** App directories live in every
  workspace (`sentin8/apps/`, `bank8/apps/`, `hotel8/apps/`, …). Hardcoding
  those paths would couple the platform to its verticals; the
  registry-key discovery keeps the host generic — a newly enrolled app is
  served the moment its key exists.
- **Same-origin bootstrap.** Flutter apps must fetch their runtime
  context (auth domain, branding, feature flags) before any user signs
  in — the SDK holds no token yet. Web proxies that call to app on the
  app's own origin so the TLS-injected instance headers establish the
  caller identity; a bearer/Shield gate there is impossible pre-login.
- **Browser runtime constraints.** Flutter WASM with threads needs
  cross-origin isolation headers; the service-worker registration shipped
  in `flutter_bootstrap.js` must be stripped for in-app webviews; SPA
  routes need an index fallback. The host centralizes these once instead
  of every app rebuilding them.

## 3. Architecture & High-Level Design

### 3.1 Two serving modes

1. **Multi-app mode (default, app-store).** The hostname decides the app:
   the slug is the first DNS label before `.{env}.{org}.{domain}`
   (e.g. `ebank.prod.ibank.local` → `ebank`). Resolution:
   `captain:app_dir:{slug}` Valkey key → absolute path of the built web
   directory. Discovery is on-the-fly — no restart needed for newly built
   or registered apps.
2. **Single-app mode (dash).** `--web-root <dir>` (or `SENTIN8_WEB_ROOT`)
   serves one fixed directory for all requests — the platform dash. The
   directory must contain `index.html` (fail-fast otherwise).

Both modes share the file-serving core, differ only in root resolution.

### 3.2 Request pipeline

```
Client ──TLS──▶ Sentin8:Proxy (L4 SNI) ──▶ sentin8:web (in-process PQ-TLS)
                                              │ context middleware
                                              │   (X-Sentin8-Hostname/App-Id)
                                              ▼
                              /v1/health ── liveness JSON
                              /v1/app/runtime/context ── proxy → sentin8:app
                              everything else ── static/SPA handler
```

- The **context middleware** injects `X-Sentin8-*` headers and strips
  client-supplied copies. Under the L4 proxy (byte-piping, no header
  injection) the HTTP `Host` header is the fallback hostname source —
  without it every proxied hostname would resolve empty.
- The **runtime-context proxy** forwards only `x-sentin8-*` and `host`
  headers to app's `GET /v1/app/runtime/context` over the platform TLS
  channel, then relays status/content-type/body unchanged. It is a public,
  pre-login surface: instance metadata only, same trust basis as the app
  shell itself.
- The **traffic counter** counts every inbound request except health
  checks and flushes to Valkey (stat ingest).

### 3.3 Static file serving invariants

- **Path traversal protection**: the requested path is resolved
  (`canonicalize`) and must stay under the resolved web root; anything
  else 404s. Encoded sequences (`..%2F`) are decoded before the check, so
  canonicalization is the enforcement point, not string matching.
- **SPA fallback**: extensionless paths serve `index.html` (client-side
  routing); missing files WITH an extension 404.
- **`flutter_bootstrap.js` patching**: the service-worker settings object
  is replaced with `{}` in memory (brace-depth scanner), the file is served
  `no-store, must-revalidate` so a rebuilt app is never shadowed by a
  cached bootstrap.
- **Cross-origin isolation**: `COOP: same-origin` and
  `COEP: require-corp` are set on the bootstrap script, `main.dart.js`,
  and `.wasm` files — required for multi-threaded WASM.

### 3.4 Policy posture

The Shield engine boots (refuse-to-start on invalid policy text) and
hot-reloads like every service — uniform enforced posture — but web has no
token-bearing handler to gate: static assets are the app shell and the
runtime-context proxy is public pre-login. The former bearer gate on the
proxy was removed (it 401'd every app bootstrap; the caller identity is
the app instance, not a user). Registration declares only the
`sentin8:web-policy` M2M workload (`sentin8:bus:consume`) for policy
hot-reload consumption.

## 4. Key Traits and Data Structures

- **`MultiAppState`**: optional Valkey connection manager for
  `captain:app_dir:{slug}` lookups. A missing connection degrades to 404s
  for app-hostname requests (health and dash mode are unaffected).
- **`SingleAppState`**: the fixed web root (dash mode).
- **`Sentin8Tls`**: in-process TLS termination, on-demand SNI cert
  resolution, context middleware — identical to every other Rust service.
- **`strip_service_worker_settings`**: a brace-depth scanner that replaces
  the `serviceWorkerSettings: {…}` object with `{}`, string-safe (a
  literal `}` inside a string does not end the object).

## 5. Rationale, Trade-offs, and Alternatives

- **Valkey keys instead of filesystem scanning or sys API calls.** The
  registry key is written once at `vertical enroll` and updated by
  deploys; lookups are O(1), require no per-request HTTP call, and keep
  web independent of sys's schema. Alternative (querying sys per request)
  added a hop on every asset; filesystem scanning reintroduced hardcoded
  vertical paths.
- **Slug = first hostname label.** The uniform hierarchy
  `{app}.{env}.{org}.{domain}` makes the first label the app slug by
  construction; no lookup table is needed. App slugs therefore must not
  contain dots (enforced at registration).
- **In-memory bootstrap patching instead of a build step.** Flutter's
  generated bootstrap carries service-worker settings that break in-app
  webviews; patching at serve time keeps app builds stock and the patch
  logic in one place. Cache-control `no-store` guarantees clients always
  get the patched body.
- **Public runtime-context proxy instead of shielding it behind auth.**
  The endpoint's response is instance metadata (auth domain, branding,
  feature flags) — nothing user-scoped — and the transport + TLS-injected
  instance headers already establish the caller. Gating it would break
  every first paint.

## 6. Internal Implementation & Invariants (Private Design)

1. **`serve_from_web_root` is the single file path** for both modes:
   decode → canonicalize → under-root check → file? stream : SPA fallback
   → 404. The unit tests pin traversal protection, SPA fallback, MIME
   handling, and the bootstrap patch.
2. **Streaming bodies** (`tokio_util::io::ReaderStream`) for non-bootstrap
   files — large WASM bundles are never fully buffered; content-length
   comes from the file metadata.
3. **Bootstrap responses are read fully and re-encoded** (they are small);
   COOP/COEP headers are attached after the body is built.
4. **Health endpoint** (`/v1/health`) returns status + build hash —
   captain monitors it; it bypasses the traffic counter.
5. **Hostname resolution falls back from `X-Sentin8-Hostname` to `Host`**
   (port stripped) because the L4 proxy does not inject headers.
6. **No tables, no migrations, no outbox — own DB identity** — the policy
   boot pool connects read-only via `SENTIN8_WEB_DATABASE_URL`
   (`sentin8_web_service` role, declared in the registration and seeded
   by `platform.sql`); the shared dev DSN is only the unset-variable
   fallback for local harnesses.
7. **Graceful shutdown** rides `Sentin8Tls::serve` (same shutdown name as
   every service); in-flight requests drain before exit.

## 7. Unresolved Questions / Future Designs

- Long-lived immutable assets (`main.dart.js`, `*.wasm`, fonts — Flutter
  fingerprints them) serve `Cache-Control: public, max-age=31536000,
  immutable`; the bootstrap stays `no-store` (a pure header change plus test).
- ETag/If-None-Match support computes `ETag: "{size}-{mtime}"` (cheap and
  change-accurate) on first stream and honors `If-None-Match` → 304 — the same
  handler change as the immutable-cache header.

---

**Design attribution:** sentin8:web and its underlying concepts were designed by Hamid Jahanzad <Hamid@Jahanzad.com>.

use crate::dns::{parse_blacklist, DnsResolver, IpCidr};
use crate::rps::RpsLimiter;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tropel_auth::AuthSigner;
// Brings `Jar::cookies` into scope — it is a method of the `CookieStore`
// TRAIT, not inherent to `Jar` — so the per-VU jar wrapper can read cookies.
use reqwest::cookie::CookieStore as _;
use tropel_sdk::types::*;

use crate::config::{HttpConfig, TlsConfig};
use tropel_sdk::Result;
use tropel_sdk::TropelError;

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MULTIPART_BOUNDARY: &str = "------------------------tropel-boundary-7a2f24b9";

/// Credential headers stripped on cross-origin redirect hops and carried
/// forward on same-origin hops. Single source of truth — the two `matches!`
/// sites below must not drift (drift in exactly this kind of list is what
/// caused the signed-Authorization-dropped-on-redirect bug).
const CREDENTIAL_HEADERS: [&str; 8] = [
    "authorization",
    "cookie",
    "cookie2",
    "proxy-authorization",
    "www-authenticate",
    "x-amz-date",
    "x-amz-content-sha256",
    "x-amz-security-token",
];

fn is_credential_header(key: &str) -> bool {
    CREDENTIAL_HEADERS.contains(&key)
}

/// Convert an execution error into [`TropelError::Http`], joining the FULL
/// error `source()` chain into the message. reqwest folds DNS-layer failures
/// — including this crate's `blacklistIPs` resolver
/// ("all resolved addresses for 'host' are blacklisted") — into the request
/// error, whose `Display` alone drops that cause. Walking the chain lets
/// proxy-style consumers (the KnockPort relay maps a blacklisted hop to a
/// clean 403 "target address is blocked") detect the root cause from the
/// message string instead of every blocked target degrading to a generic
/// "upstream request failed".
fn http_request_error<E: std::error::Error + 'static>(e: &E) -> TropelError {
    let mut msg = format!("Request failed: {e}");
    let mut src = std::error::Error::source(e);
    for _ in 0..8 {
        let Some(s) = src else { break };
        msg.push_str(" -> ");
        msg.push_str(&s.to_string());
        src = std::error::Error::source(s);
    }
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = source {
        if err.downcast_ref::<h2::Error>().is_some() {
            return TropelError::Http2(msg);
        }
        source = std::error::Error::source(err);
    }
    TropelError::Http(msg)
}

/// Canonicalize an HTTP header name to Go's MIME canonical form
/// (uppercase first letter of each dash-separated word, lowercase the
/// rest): `content-type` → `Content-Type`, `x-request-id` →
/// `X-Request-Id`. The `http`/reqwest crate lowercases every `HeaderName`,
/// so without this every k6/Postman doc idiom (`res.headers['Content-Type']`,
/// `pm.response.header('Content-Type')`) would see `undefined`.
///
/// Format reqwest::Version as a human-readable protocol string.
fn format_http_version(v: reqwest::Version) -> String {
    let debug = format!("{:?}", v);
    if debug.starts_with("Http(") {
        let inner = &debug[5..debug.len() - 1];
        let parts: Vec<&str> = inner.split(", ").collect();
        if parts.len() == 2 {
            return format!("HTTP/{}.{}", parts[0], parts[1]);
        }
    }
    debug
}

fn canonical_header_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut upper = true;
    for c in name.chars() {
        if c == '-' {
            out.push('-');
            upper = true;
        } else if upper {
            out.push(c.to_ascii_uppercase());
            upper = false;
        } else {
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

/// Per-certificate-identity HTTP clients (`Arc<Mutex<…>>` because
/// `std::sync::Mutex` is not `Clone` while `HttpClient` derives `Clone`).
type CertClientMap = Arc<Mutex<HashMap<(String, String, bool), reqwest::Client>>>;

/// Per-VU HTTP client with auth and response tracking.
#[derive(Clone)]
pub struct HttpClient {
    /// HTTP/2 connection lanes (TR-303): `config.http2_connections`
    /// independent reqwest::Client instances, each with its OWN connection
    /// pool. hyper runs a single h2 connection per pool, so N concurrent
    /// streams beyond the server's MAX_CONCURRENT_STREAMS queue on the one
    /// connection. N lanes = N h2 connections = stream acquisition
    /// parallelized. Round-robin via `next_lane`.
    inner: Vec<reqwest::Client>,
    /// Twin clients that never follow redirects (`Policy::none()`), used when
    /// a request sets `follow_redirects: false` (reqwest bakes the redirect
    /// policy into the client at build time, so per-request redirect control
    /// needs a second client). `None` when `max_redirects == 0` — the primary
    /// client already never follows.
    no_redirect: Option<Vec<reqwest::Client>>,
    /// Round-robin lane cursor (Arc so clones — one per VU — share the cursor).
    next_lane: Arc<std::sync::atomic::AtomicUsize>,
    /// Lazily-built clients for per-request mTLS identities, keyed by
    /// `(cert_path, key_path, follow_redirects)`. The identity is baked into
    cert_clients: CertClientMap,
    /// Config snapshot used to lazily build per-certificate clients.
    config: HttpConfig,
    /// TLS snapshot used to lazily build per-certificate clients.
    tls: TlsConfig,
    /// When true, response bodies are discarded entirely.
    /// The body field will be empty, saving memory and bandwidth.
    discard_bodies: bool,
    /// Optional global RPS limiter (k6 `options.rps`), shared across all
    /// VUs of the run. `None` = unlimited.
    rps: Option<Arc<RpsLimiter>>,
    /// Log every HTTP request/response (method, URL, status, timing) at
    /// debug level — the `--http-debug` flag / `HttpConfig.http_debug`.
    http_debug: bool,
    /// `--http-debug=full` — also log request/response bodies.
    http_debug_full: bool,
    /// k6 `blacklistIPs` CIDRs, parsed once at client build. Hostnames are
    /// filtered by the DNS resolver, but an IP-literal URL (e.g.
    /// `http://127.0.0.1:8080`) never triggers a lookup — reqwest hands the
    /// literal straight to the connector. This list backs the per-hop
    /// literal check in `execute()` so a blacklisted literal (including a
    /// redirect target) is rejected before any connection attempt.
    blacklist: Vec<IpCidr>,
}

impl HttpClient {
    /// Create a new HTTP client from config (default TLS settings).
    pub fn new(config: &HttpConfig) -> Result<Self> {
        Self::with_tls(config, &TlsConfig::default())
    }

    /// Create a client with an optional global RPS limiter (no TLS overrides).
    pub fn new_with_rps(config: &HttpConfig, rps: Option<Arc<RpsLimiter>>) -> Result<Self> {
        Self::with_tls_and_rps(config, &TlsConfig::default(), rps)
    }

    /// Create a new HTTP client from config, applying the TLS settings:
    /// - `insecure_skip_verify`: disable certificate verification
    ///   (`danger_accept_invalid_certs`)
    /// - `min_version` / `max_version`: TLS protocol version bounds
    /// - `client_cert` + `client_key`: mTLS client identity — an unencrypted
    ///   PEM cert + key pair, concatenated into one buffer for
    ///   `Identity::from_pem` (accepts PKCS#8, PKCS#1 and SEC1 keys).
    ///
    /// `client_passphrase` and `allowed_ciphers` are deliberately not applied:
    /// PKCS#12/encrypted-key support requires the native-tls backend, and
    /// per-client cipher selection is not exposed through reqwest's
    /// `ClientBuilder` (custom cipher suites would need
    /// `use_preconfigured_tls(rustls::ClientConfig)`). This build uses
    /// rustls, which negotiates a safe default cipher set; a supplied
    /// passphrase logs a warning (its value is never logged).
    pub fn with_tls(config: &HttpConfig, tls: &TlsConfig) -> Result<Self> {
        Self::with_tls_and_rps(config, tls, None)
    }

    /// Full constructor: TLS overrides plus an optional shared global RPS
    /// limiter (k6 `options.rps`). The limiter is created once per run and
    /// cloned into every per-VU client so the cap is global across VUs.
    pub fn with_tls_and_rps(
        config: &HttpConfig,
        tls: &TlsConfig,
        rps: Option<Arc<RpsLimiter>>,
    ) -> Result<Self> {
        let identity = Self::load_global_identity(tls)?;
        // Both clients use Policy::none() because Tropel always manually
        // follows redirects (k6 parity — every hop counts as a request).
        // The old code built `inner` with Policy::limited(N), but tracing
        // every path shows it was NEVER selected when max_redirects > 0 —
        // dead weight: an extra ClientConfig + pool + DNS resolver + TLS
        // context + resumption cache. Building both with Policy::none()
        // eliminates that waste. The no_redirect twin is still needed for
        // per-request `follow_redirects: false` when max_redirects > 0.
        // TR-303: build `http2_connections` lanes (default 1 — a single
        // client, preserving the pre-lane behaviour). Each lane is an
        // independent reqwest::Client with its own pool, so an h2 server with
        // a low MAX_CONCURRENT_STREAMS sees N parallel connections instead of
        // N streams queued on one. The no_redirect twin mirrors the lanes.
        let lane_count = config.http2_connections.max(1);
        let inner: Vec<reqwest::Client> = (0..lane_count)
            .map(|_| {
                Self::build_client(
                    config,
                    tls,
                    identity.clone(),
                    reqwest::redirect::Policy::none(),
                )
            })
            .collect::<Result<_>>()?;
        let no_redirect = if config.max_redirects > 0 {
            Some(inner.clone())
        } else {
            None
        };

        Ok(Self {
            inner,
            no_redirect,
            next_lane: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            cert_clients: Arc::new(Mutex::new(HashMap::new())),
            config: config.clone(),
            tls: tls.clone(),
            discard_bodies: config.discard_response_bodies,
            rps,
            http_debug: config.http_debug,
            http_debug_full: config.http_debug_full,
            blacklist: parse_blacklist(&config.blacklist_ips),
        })
    }

    /// Build a `reqwest::Client` from the full HTTP/TLS configuration with an
    /// optional mTLS identity and an explicit redirect policy. This is the
    /// single builder shared by the primary client, the no-redirect twin, and
    /// the lazily-built per-request-certificate clients.
    fn build_client(
        config: &HttpConfig,
        tls: &TlsConfig,
        identity: Option<reqwest::Identity>,
        redirect: reqwest::redirect::Policy,
    ) -> Result<reqwest::Client> {
        // k6 `noConnectionReuse`: close the connection after every request.
        // reqwest has no direct "reuse off" switch — setting the idle pool to
        // 0 causes every returned connection to be closed instead of pooled,
        // so each request opens a fresh connection.
        let max_idle = if config.no_connection_reuse {
            0
        } else {
            config.max_idle_connections
        };
        // Global request timeout: configurable via `HttpConfig.request_timeout`
        // (k6 `timeout`); falls back to the 60s engine default (k6 parity —
        // k6's `Params.timeout` defaults to 60s, TR-230). A per-request
        // `timeout` (Request.timeout) can still override it shorter.
        let request_timeout = config
            .request_timeout
            .as_deref()
            .and_then(|s| parse_duration(s).ok())
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT);
        let mut builder = reqwest::Client::builder()
            // NO built-in cookie store. Clones of a reqwest::Client share one
            // store (and one pool), so a store here would leak ONE global jar
            // across every VU — the cross-VU cookie leak this module's
            // per-VU `VuCookieClient` wrapper removes. The wrapper is the
            // sole cookie authority: `execute_with_jar` injects jar cookies
            // per hop and stores every Set-Cookie header back into the jar.
            .user_agent(&config.user_agent)
            .pool_max_idle_per_host(max_idle)
            .timeout(request_timeout);

        // ── TLS: insecure_skip_verify ──
        if tls.insecure_skip_verify {
            builder = builder.danger_accept_invalid_certs(true);
        }

        // ── TLS: min/max protocol version ──
        if let Some(version) = parse_tls_version(&tls.min_version) {
            builder = builder.min_tls_version(version);
        }
        if let Some(version) = parse_tls_version(&tls.max_version) {
            builder = builder.max_tls_version(version);
        }

        // ── TLS: mTLS client identity ──
        if let Some(identity) = identity {
            builder = builder.identity(identity);
        }

        if !config.decompress {
            builder = builder.no_deflate();
            builder = builder.no_gzip();
            builder = builder.no_brotli();
        }

        builder = builder.redirect(redirect);

        if let Some(timeout_str) = &config.keep_alive {
            if let Ok(timeout) = parse_duration(timeout_str) {
                builder = builder.pool_idle_timeout(timeout);
            }
        }

        // TCP keep-alive: `idle_connection_timeout` controls the socket-level
        // keep-alive idle period (probes sent after this much idle). This is
        // distinct from the connection-pool idle timeout above.
        if let Some(timeout_str) = &config.idle_connection_timeout {
            if let Ok(timeout) = parse_duration(timeout_str) {
                builder = builder.tcp_keepalive(timeout);
            }
        }

        // HTTP/2 toggle: when disabled, force HTTP/1.1. When enabled (default)
        // reqwest negotiates HTTP/2 over TLS via ALPN. For plaintext (http://)
        // connections, reqwest uses HTTP/1.1 unless http2_prior_knowledge() is
        // called — which is NOT done here, so plaintext is always HTTP/1.1.
        if !config.http2 {
            builder = builder.http1_only();
        } else {
            // Line 372: h2 tuning — the highest-value knob for long-running
            // connections. Without keepalive PINGs a dead-but-open h2
            // connection stalls every VU silently (k6 sends no h2 PINGs at all).
            builder = builder
                .http2_keep_alive_interval(Some(std::time::Duration::from_secs(10)))
                .http2_keep_alive_timeout(std::time::Duration::from_secs(5))
                .http2_keep_alive_while_idle(true)
                .http2_adaptive_window(true)
                // 16 MiB connection window: shared by all streams on one h2
                // conn. The default (65 KiB) is the binding constraint at
                // 100+ concurrent streams — this is the single biggest h2
                // throughput knob after connection count.
                .http2_initial_connection_window_size(16 * 1024 * 1024);
        }

        // DNS resolver: k6-compatible options (hosts map, blacklist, TTL
        // cache, select/policy) on top of real timed lookups.
        let dns_resolver = DnsResolver::from_config(config);
        // - `connector_layer` times each connection attempt (DNS + TCP + TLS)
        //   via generic tower middleware that never names reqwest's sealed
        //   `Unnameable`/`Conn` types.
        // Results are recorded on the VU thread and consumed by `execute()`.
        builder = builder
            .dns_resolver(dns_resolver)
            .connector_layer(crate::subtimings::TimingConnectorLayer);

        builder
            .build()
            .map_err(|e| TropelError::Http(format!("Failed to create HTTP client: {}", e)))
    }

    /// Load the global mTLS identity from `TlsConfig` (if configured).
    fn load_global_identity(tls: &TlsConfig) -> Result<Option<reqwest::Identity>> {
        if let Some(cert_path) = &tls.client_cert {
            let key_path = tls.client_key.as_deref().ok_or_else(|| {
                TropelError::Config(format!(
                    "TLS client_cert '{}' set but no client_key: a client \
                     identity requires both a certificate and its private key",
                    cert_path
                ))
            })?;
            Ok(Some(Self::load_pem_identity(
                cert_path,
                key_path,
                tls.client_passphrase.as_deref(),
            )?))
        } else {
            if tls.client_key.is_some() {
                tracing::warn!("client_key is set without client_cert — the key will be ignored");
            }
            Ok(None)
        }
    }

    /// Read a PEM cert + key pair from disk and build a `reqwest::Identity`.
    ///
    /// Concatenates cert + key into ONE PEM buffer and uses
    /// `Identity::from_pem` (the only identity constructor available under the
    /// rustls feature). It parses mixed PEM sections and accepts PKCS#8
    /// (`BEGIN PRIVATE KEY`), PKCS#1 (`BEGIN RSA PRIVATE KEY`) and SEC1
    /// (`BEGIN EC PRIVATE KEY`) keys. PKCS#12 bundles and encrypted PEM keys
    /// require the native-tls backend; a supplied passphrase logs a warning
    /// (its value is never logged) and the key must be unencrypted PEM.
    fn load_pem_identity(
        cert_path: &str,
        key_path: &str,
        passphrase: Option<&str>,
    ) -> Result<reqwest::Identity> {
        if passphrase.is_some() {
            tracing::warn!(
                "client_passphrase is only honored with the native-tls backend; \
                 this rustls build uses unencrypted PEM keys, so the supplied \
                 passphrase will be ignored"
            );
        }
        let cert_bytes = std::fs::read(cert_path).map_err(|e| {
            TropelError::Config(format!("Failed to read client cert '{}': {}", cert_path, e))
        })?;
        let key_bytes = std::fs::read(key_path).map_err(|e| {
            TropelError::Config(format!("Failed to read client key '{}': {}", key_path, e))
        })?;
        let mut combined = cert_bytes;
        combined.extend_from_slice(b"\n");
        combined.extend_from_slice(&key_bytes);
        reqwest::Identity::from_pem(&combined).map_err(|e| {
            TropelError::Config(format!(
                "Failed to load PEM client identity (cert '{}', key '{}'): {}",
                cert_path, key_path, e
            ))
        })
    }

    /// Backlog line 426: pre-warm connections during the serial startup window.
    ///
    /// Send a lightweight HEAD request to each distinct host to trigger DNS
    /// resolution and TCP+TLS handshake before VUs start ramping. The
    /// connection pool caches the result so the first real request skips the
    /// cold-start penalty (~50–150 ms per host on TLS). This runs during the
    /// `start_delay` dead time — zero overhead once VUs are live.
    ///
    /// Failures are logged but never fatal — pre-warming is best-effort.
    pub async fn pre_warm(&self, urls: &[String]) {
        use std::collections::HashSet;
        // Deduplicate by (scheme, host, port) — only one connection per
        // distinct origin needs the handshake.
        let mut seen = HashSet::new();
        let mut warm_urls = Vec::new();
        for url in urls {
            if let Ok(parsed) = reqwest::Url::parse(url) {
                let key = (
                    parsed.scheme().to_string(),
                    parsed.host_str().unwrap_or("").to_string(),
                    parsed.port_or_known_default().unwrap_or(80),
                );
                if seen.insert(key) {
                    warm_urls.push(url.clone());
                }
            }
        }
        if warm_urls.is_empty() {
            return;
        }
        tracing::info!(
            "Pre-warming {} distinct host(s) during startup",
            warm_urls.len()
        );
        // Fire all HEAD requests concurrently — bounded to avoid fd
        // exhaustion on high-host-count configs.
        let limit = warm_urls.len().min(32);
        let semaphore = Arc::new(tokio::sync::Semaphore::new(limit));
        let mut handles = Vec::with_capacity(warm_urls.len());
        for url in warm_urls {
            let sem = semaphore.clone();
            // Pre-warm lane 0 only — all lanes share the DNS/TLS machinery;
            // the other lanes warm on first use.
            let client = self.inner[0].clone();
            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire().await;
                let resp = client
                    .head(&url)
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await;
                match resp {
                    Ok(r) => {
                        tracing::debug!("Pre-warm {} → {}", url, r.status());
                        // Drop the response to release the connection back
                        // to the pool (the pool keeps the TLS session).
                    }
                    Err(e) => {
                        tracing::debug!(
                            "Pre-warm {} failed: {} — will retry on first request",
                            url,
                            e
                        );
                    }
                }
            }));
        }
        // Wait for all pre-warm requests to complete (or timeout).
        for h in handles {
            let _ = h.await;
        }
        tracing::info!("Pre-warm complete — connections ready for VU start");
    }

    /// Pick the `reqwest::Client` for a request, honoring the per-request
    /// `follow_redirects` and `certificate` overrides that reqwest bakes in at
    /// client-build time:
    /// - `follow_redirects: false` → the no-redirect twin client
    /// - `certificate` → a lazily-built client with that mTLS identity,
    ///   cached per (cert, key, follow_redirects) so each distinct identity
    ///   is loaded from disk exactly once
    ///
    /// Note: the `cert_clients` lock is held across `load_pem_identity` (file
    /// reads) and `build_client`. This is safe because clients are per-VU and
    /// run on single-threaded current-thread runtimes (no awaits under the
    /// lock, no concurrent callers for the same `HttpClient`). A future
    /// refactor that shares one client across threads must build outside the
    /// lock instead.
    fn select_client(&self, request: &Request) -> Result<reqwest::Client> {
        self.select_client_with_follow(request, request.follow_redirects)
    }

    /// Like [`select_client`], but with an explicit follow decision. Used by
    /// `execute()` for manual redirect following: when Tropel follows
    /// redirects ITSELF (k6 parity — every hop counts as a request), it needs
    /// a `Policy::none()` client so reqwest never auto-follows behind its
    /// back; the no-redirect twin provides exactly that.
    fn select_client_with_follow(
        &self,
        request: &Request,
        follow: bool,
    ) -> Result<reqwest::Client> {
        // `--no-redirects` (HttpConfig.no_redirects) forces no-follow for
        // EVERY request, regardless of the per-request flag: the 3xx is
        // returned as-is (k6 always follows; this opt-out is a Tropel extra).
        let follow = follow && !self.config.no_redirects;
        match &request.certificate {
            Some(cert) => {
                let cert_path = cert.cert.as_deref().ok_or_else(|| {
                    TropelError::Config("per-request certificate requires a cert path".into())
                })?;
                let key_path = cert.key.as_deref().ok_or_else(|| {
                    TropelError::Config("per-request certificate requires a key path".into())
                })?;
                let cache_key = (cert_path.to_string(), key_path.to_string(), follow);
                // Poison-tolerant: a single panicked thread must not disable
                // the cert-client cache for the whole run (backlog P3).
                let mut cache = self.cert_clients.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(client) = cache.get(&cache_key) {
                    return Ok(client.clone());
                }
                let identity =
                    Self::load_pem_identity(cert_path, key_path, cert.passphrase.as_deref())?;
                let redirect = if follow && self.config.max_redirects > 0 {
                    reqwest::redirect::Policy::limited(self.config.max_redirects as usize)
                } else {
                    reqwest::redirect::Policy::none()
                };
                let client = Self::build_client(&self.config, &self.tls, Some(identity), redirect)?;
                cache.insert(cache_key, client.clone());
                Ok(client)
            }
            None => {
                if follow {
                    Ok(self.pick_lane(&self.inner).clone())
                } else if let Some(no_redirect) = &self.no_redirect {
                    Ok(self.pick_lane(no_redirect).clone())
                } else {
                    Ok(self.pick_lane(&self.inner).clone())
                }
            }
        }
    }

    /// Round-robin lane selection (TR-303). A single `HttpClient` is shared
    /// per VU, so a global atomic cursor spreads load across the
    /// `http2_connections` pools regardless of VU count.
    fn pick_lane<'a>(&self, lanes: &'a [reqwest::Client]) -> &'a reqwest::Client {
        if lanes.len() == 1 {
            return &lanes[0];
        }
        let idx = self
            .next_lane
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % lanes.len();
        &lanes[idx]
    }

    /// Execute an HTTP request with sub-timing instrumentation.
    ///
    /// Measures the full request lifecycle with real phase data captured via
    /// reqwest's `dns_resolver` and `connector_layer` hooks (see
    /// [`crate::subtimings`]):
    /// - **blocked**: request start → connector `call()` begins (pool wait /
    ///   queueing; zero when a pooled keep-alive connection is reused)
    /// - **dns**: real DNS resolution time
    /// - **connecting**: connector call minus DNS (pure TCP for http; for
    ///   https reqwest folds the TLS handshake into the connector call, so it
    ///   is included here)    /// - **waiting** (TTFB): from the request being sent to response headers
    ///   received, EXCLUDING the connection phases (blocked + dns + connecting)
    ///   — subtracted from the raw elapsed so the breakdown sums to `total`,
    ///   matching k6's `http_req_waiting` semantics. HTTP/2 stream admission
    ///   queueing is included here because reqwest does not expose it as a
    ///   separate phase; it is a documented transport limitation.
    /// - **receiving**: from response headers to full body bytes received
    /// - **total**: entire `execute()` duration
    ///
    /// `tls_handshaking` and `sending` remain `Duration::ZERO` — reqwest
    /// seals those phases inside the connector / request future. A
    /// hyper-based custom connector would be required to split them out.
    ///
    /// Returns the response along with the number of bytes sent in the request body.
    pub async fn execute(
        &self,
        request: &Request,
        signer: Option<&dyn AuthSigner>,
    ) -> Result<HttpResponse> {
        self.execute_with_jar(request, signer, None).await
    }

    /// Execute a request with an optional per-VU cookie jar.
    ///
    /// When `jar` is `Some` (the per-VU [`VuCookieClient`] path), stored
    /// cookies for each hop's URL are injected into the request — unless the
    /// request carries an explicit `Cookie` header, which wins (k6:
    /// `params.headers.Cookie` overrides the jar) — and every `Set-Cookie`
    /// header on each hop response is stored back into the jar. `None` (the
    /// plain [`execute`] path) performs no cookie handling.
    pub(crate) async fn execute_with_jar(
        &self,
        request: &Request,
        signer: Option<&dyn AuthSigner>,
        jar: Option<&reqwest::cookie::Jar>,
    ) -> Result<HttpResponse> {
        // RPS pacing is done per-hop inside the redirect loop (line 580)
        // so each redirect hop is rate-limited. Without this, rps:1000
        // against a 302 chain sends 2000/s (backlog line 240).

        // Serialize the request body ONCE. The resulting bytes feed BOTH the
        // data_sent accounting (exact wire size) and the reqwest body — the
        // old code called body_size() and body_to_reqwest() separately, each
        // re-serializing JSON / urlencoded / multipart bodies (2× work per
        // request on the hot path). The bytes are memcpy'd into reqwest
        // (cheap); the expensive serialization happens exactly once.
        let body_bytes: Option<Vec<u8>> = request.body.as_ref().map(body_to_bytes);
        let request_body_size: u64 = body_bytes.as_ref().map_or(0, |b| b.len() as u64);

        if self.http_debug {
            // info! so the flag is self-sufficient: the default log filter is
            // WARN, and a debug-level line would only appear with RUST_LOG.
            if self.http_debug_full {
                let body_preview = body_bytes
                    .as_deref()
                    .map(|b| String::from_utf8_lossy(&b[..b.len().min(1024)]).into_owned())
                    .unwrap_or_else(|| "(no body)".to_string());
                let headers: Vec<String> = request
                    .headers
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k, v))
                    .collect();
                tracing::info!(
                    "HTTP >>> {:?} {} headers={:?} body={:?}",
                    request.method,
                    request.url,
                    headers,
                    body_preview
                );
            } else {
                tracing::info!(
                    "HTTP >>> {:?} {} (body {} bytes, {} headers)",
                    request.method,
                    request.url,
                    request_body_size,
                    request.headers.len()
                );
            }
        }

        // Build the reqwest request
        let multipart_content_type = if matches!(request.body, Some(Body::FormData(_))) {
            Some(format!(
                "multipart/form-data; boundary={}",
                MULTIPART_BOUNDARY
            ))
        } else {
            None
        };

        // ── Redirect handling — k6 parity ──
        // k6 counts EVERY redirect hop as its own request (the test.k6.io 302
        // chain produced 136 http_reqs for 68 iterations; Tropel recorded only
        // the final 64). We therefore follow redirects MANUALLY: each 3xx hop
        // becomes its own HttpResponse collected into `redirects`, and the
        // returned response is the FINAL one (what scripts see via
        // pm.response / res), with `redirects` attached for per-hop sample
        // emission by callers.
        //
        // `--no-redirects` (HttpConfig.no_redirects) disables following
        // entirely: the 3xx response is returned as-is with no hops captured
        // — an option k6 itself lacks (it always follows up to maxRedirects).
        let manual_follow =
            request.follow_redirects && !self.config.no_redirects && self.config.max_redirects > 0;
        let client = if manual_follow {
            // Manual following needs a Policy::none() client so reqwest never
            // auto-follows behind our back (the no-redirect twin).
            self.select_client_with_follow(request, false)?
        } else {
            self.select_client(request)?
        };
        let max_hops = if manual_follow {
            self.config.max_redirects as usize
        } else {
            0
        };
        let mut redirects: Vec<HttpResponse> = Vec::new();
        let mut current_url = request.url.clone();
        let mut current_method = request.method.clone();
        let mut current_body: Option<Vec<u8>> = body_bytes.clone();
        let mut hop_index: usize = 0;
        // Set when the previous hop redirected to a DIFFERENT origin: the
        // next hop must not carry credentials (Authorization/Cookie/…),
        // matching reqwest's and k6's redirect policies.
        let mut strip_sensitive = false;
        // Headers the signer ADDED to the hop-0 request (Authorization,
        // x-amz-*, …). `request.headers` is never mutated by signers — they
        // sign the built reqwest::Request in place — so without this capture
        // the signed header dies with hop 0's consumed request and a
        // same-origin redirect 401s (reqwest's auto-follow used to forward
        // it for us).
        let mut signed_headers: Vec<(String, String)> = Vec::new();

        loop {
            // Backlog line 240: RPS pacing per hop — each redirect hop
            // must be rate-limited so rps:1000 against a 302 chain doesn't
            // send 2000/s.
            if let Some(limiter) = &self.rps {
                limiter.acquire().await;
            }
            // Each redirect hop is checked too — a Location header pointing
            // at a blacklisted literal must not slip past the resolver.
            check_literal_blacklist(&self.blacklist, &current_url)?;
            // Line 454 + TR-312: parse the URL ONCE per hop and reuse the
            // parsed Url for BOTH the reqwest builder and the cookie jar.
            // The old code let reqwest re-parse the string internally AND
            // parsed it again for the jar — two parses per hop (three with
            // the redirect rewrite).
            let hop_url = reqwest::Url::parse(&current_url)
                .map_err(|e| TropelError::Http(format!("Invalid URL '{}': {}", current_url, e)))?;
            // The per-hop timing slot is created BEFORE the request body so the
            // timed body wrapper can record the request-write phase into it.
            // request_start is stamped later, just before execute() (see below).
            let slot = crate::subtimings::new_slot();

            // Build the reqwest request for THIS hop (URL/method/body may
            // have been rewritten by a redirect). Match by reference: the
            // `Custom` arm binds `m: &String`, so `current_method` is NOT
            // moved out of — it is still needed by the redirect-rewrite
            // logic below (303 → GET etc.).
            // Backlog line 140: the body was attached ONLY for POST/PUT/PATCH,
            // so DELETE/OPTIONS/TRACE and custom-method bodies were silently
            // dropped. The builder is now method-agnostic — any method gets
            // whatever body it is given (Postman's GET/HEAD pruning lives in
            // the collection parser via protocolProfileBehavior
            // .disableBodyPruning). HEAD/CONNECT edge cases: reqwest tolerates
            // a body on HEAD; CONNECT is rejected below.
            let mut req_builder = match &current_method {
                Method::GET => client.get(hop_url.clone()),
                Method::POST => client.post(hop_url.clone()),
                Method::PUT => client.put(hop_url.clone()),
                Method::PATCH => client.patch(hop_url.clone()),
                Method::DELETE => client.delete(hop_url.clone()),
                Method::HEAD => client.head(hop_url.clone()),
                Method::OPTIONS => client.request(reqwest::Method::OPTIONS, hop_url.clone()),
                Method::TRACE => client.request(reqwest::Method::TRACE, hop_url.clone()),
                Method::CONNECT => {
                    return Err(TropelError::Http("CONNECT method not supported".into()));
                }
                // Custom token (PURGE, LINK, …): parse into a reqwest Method.
                // `from_bytes` validates the token charset, so a token that
                // slipped past Method::parse would surface as a request-build
                // error here instead of silently becoming GET.
                Method::Custom(m) => {
                    let method = reqwest::Method::from_bytes(m.as_bytes()).map_err(|e| {
                        TropelError::Http(format!("Invalid HTTP method '{}': {}", m, e))
                    })?;
                    client.request(method, hop_url.clone())
                }
            };
            if let Some(bytes) = &current_body {
                // TR-202: wrap the body in a timed body so `sending` is a REAL
                // measurement (the wire-write time of the request body), not a
                // hardcoded 0. The wrapper preserves Content-Length (exact size
                // hint), so the wire format is unchanged. The `sending` phase
                // is recorded into this hop's slot; k6's tracer.go subtleties
                // are applied in the timings assembly (see `k6_done`).
                //
                // Gated on `signer.is_none()`: the Digest challenge-response
                // retry re-sends the request via `built_request.try_clone()`,
                // which returns None for a streaming (wrapped) body — wrapping
                // a signed request would silently disable the 401 retry.
                if signer.is_none() {
                    let timed =
                        crate::subtimings::TimedBody::new(bytes.clone().into(), slot.clone());
                    req_builder = req_builder.body(reqwest::Body::wrap(timed));
                } else {
                    req_builder = req_builder.body(reqwest::Body::from(bytes.clone()));
                }
            }

            // Add headers
            if let Some(content_type) = &multipart_content_type {
                if !request
                    .headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                {
                    req_builder = req_builder.header("Content-Type", content_type);
                }
            } else if let Some(body) = &request.body {
                // Backlog line 240: default Content-Type for body types that
                // need one. Without this, OAuth1's is_form_urlencoded returns
                // false and form params are omitted from the signature base.
                if !request
                    .headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                {
                    let ct = match body {
                        Body::Json(_) => "application/json",
                        Body::UrlEncoded(_) => "application/x-www-form-urlencoded",
                        Body::GraphQL { .. } => "application/json",
                        Body::Binary(_) => "application/octet-stream",
                        Body::FormData(_) => unreachable!("handled above"),
                        Body::Raw(_) => "text/plain",
                    };
                    req_builder = req_builder.header("Content-Type", ct);
                }
            }
            for (key, value) in &request.headers {
                // Cross-origin redirect hops must not leak credentials to
                // another origin (reqwest's redirect policy strips
                // Authorization/Cookie/etc on origin change).
                if strip_sensitive && is_credential_header(&key.to_ascii_lowercase()) {
                    continue;
                }
                // TR-230: k6 sets req.Host (a field), never a Host header.
                // The host override lives in `request.host`; a stray Host
                // header from another path must not double up.
                if key.eq_ignore_ascii_case("host") {
                    continue;
                }
                req_builder = req_builder.header(key.as_str(), value.as_str());
            }
            // TR-230: the k6 Host override rides on the wire as the Host
            // header (reqwest honors a user-set Host header over the URL's).
            if let Some(host) = &request.host {
                req_builder = req_builder.header(reqwest::header::HOST, host.as_str());
            }
            // Backlog line 246: On redirect hops WITH a signer, skip replaying
            // stale signed_headers — the signing block below will re-sign the
            // request for the new URL. Without a signer (or on hop 0), replay
            // the captured headers as before. Cross-origin hops always strip.
            // Note: replay is done post-build via insert (replace) not append,
            // so a redirected request does not accumulate duplicate Authorization.
            let replay_signed_headers =
                !strip_sensitive && signer.is_none() && !signed_headers.is_empty();

            // ── Per-VU cookie jar: inject stored cookies for this hop ──
            // Skipped on cross-origin redirect hops (credentials must not
            // leak to another origin — the same rule as the `strip_sensitive`
            // header check above). `Jar::cookies` applies domain/path/secure
            // rules for the hop URL.
            //
            // TR-230: k6's SetRequestCookies merge. When the request carries
            // structured `request.cookies` (k6 params.cookies), the jar
            // cookies are sent ALONGSIDE them — a `replace:false` (default)
            // request cookie coexists with the jar cookie of the same name,
            // while `replace:true` suppresses the jar's. The script's manual
            // Cookie header (params.headers) is kept first. Without
            // request.cookies, the old rule holds: an explicit Cookie header
            // wins and the jar is skipped.
            if !strip_sensitive {
                if let Some(jar) = jar {
                    if !request.cookies.is_empty() {
                        let mut parts: Vec<String> = Vec::new();
                        if let Some((_, manual)) = request
                            .headers
                            .iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case("cookie"))
                        {
                            parts.push(manual.clone());
                        }
                        if let Some(jar_value) = jar.cookies(&hop_url) {
                            for (name, value) in
                                parse_cookie_header_value(jar_value.to_str().unwrap_or(""))
                            {
                                let replaced = request
                                    .cookies
                                    .iter()
                                    .any(|c| c.replace && c.name.eq_ignore_ascii_case(&name));
                                if !replaced {
                                    parts.push(format!("{name}={value}"));
                                }
                            }
                        }
                        for c in &request.cookies {
                            parts.push(format!("{}={}", c.name, c.value));
                        }
                        if !parts.is_empty() {
                            req_builder =
                                req_builder.header(reqwest::header::COOKIE, parts.join("; "));
                        }
                    } else {
                        let has_explicit_cookie = request
                            .headers
                            .iter()
                            .any(|(k, _)| k.eq_ignore_ascii_case("cookie"));
                        if !has_explicit_cookie {
                            if let Some(value) = jar.cookies(&hop_url) {
                                req_builder = req_builder.header(reqwest::header::COOKIE, value);
                            }
                        }
                    }
                }
            }

            // Add query parameters. ONLY on hop 0: the original request's
            // query_params describe the ORIGINAL URL. A redirect target URL
            // (Location header) carries its own query — re-appending the
            // original params there turns /x?page=2 → 302 /y?token=z into
            // /y?token=z&page=2 (k6/reqwest don't do this).
            if hop_index == 0 && !request.query_params.is_empty() {
                // Backlog line 143: query_params is a HashMap with RandomState,
                // so reqwest's query serialization was nondeterministic
                // run-to-run (breaks body-signing prerequest scripts and
                // byte-reproducibility). Sort by key so the wire bytes are
                // stable for identical inputs.
                let mut sorted: Vec<(String, String)> = request
                    .query_params
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                sorted.sort();
                req_builder = req_builder.query(&sorted);
            }

            // Set timeout (client-level timeout is already set, request can override shorter)
            if let Some(timeout) = request.timeout {
                req_builder = req_builder.timeout(timeout);
            }

            // Build the request, then apply auth IN PLACE. Signers need the
            // final method/URL/body (SigV4, OAuth1, Hawk), which a
            // RequestBuilder cannot expose, so the auth happens on the built
            // Request. Auth is applied on EVERY hop (not just hop 0) so the
            // signature is valid for the current method+URL. The signer-added
            // headers are captured at hop 0 for signer-less replay on hops
            // where no signer is available; cross-origin hops strip them.
            let mut built_request = req_builder
                .build()
                .map_err(|e| TropelError::Http(format!("Failed to build request: {}", e)))?;
            // Replay captured credential headers on same-origin, signer-less
            // redirect hops via insert (replace) — not append — so stale
            // Authorization/Cookie is not duplicated (reqwest's builder
            // header() is append, see request.rs:226).
            if replay_signed_headers {
                for (key, value) in signed_headers.iter().filter(|(k, _)| k != "cookie") {
                    if let Ok(name) = reqwest::header::HeaderName::from_bytes(key.as_bytes()) {
                        if let Ok(val) = value.parse() {
                            built_request.headers_mut().insert(name, val);
                        }
                    }
                }
            }
            // Backlog line 246: Sign on EVERY hop (not just hop 0) so the
            // Authorization signature is valid for the current method+URL.
            // Hop 0 captures what the signer added so the replay block above
            // can apply it on signer-less hops. Redirect hops re-sign from
            // scratch — the old signature is bound to the previous URL.
            if !strip_sensitive {
                if let Some(signer) = signer {
                    signer
                        .sign(&mut built_request)
                        .map_err(|e| TropelError::Http(format!("Auth signing failed: {}", e)))?;
                    if hop_index == 0 {
                        // Capture what the signer added/changed vs the original
                        // headers so signer-less hops can replay them.
                        for (name, value) in built_request.headers().iter() {
                            let key = name.as_str().to_ascii_lowercase();
                            if !is_credential_header(&key) {
                                continue;
                            }
                            let value_str = value.to_str().unwrap_or("");
                            let identical_in_original = request
                                .headers
                                .iter()
                                .any(|(k, v)| k.eq_ignore_ascii_case(&key) && v == value_str);
                            if !identical_in_original {
                                signed_headers.push((key, value_str.to_string()));
                            }
                        }
                    }
                }
            }

            // Keep a clone for the Digest challenge-response retry below. For
            // all other signers `challenge_response` returns None and this is
            // unused.
            let retry_request = built_request.try_clone();

            // ═══════════════════════════════════════════════════════
            // Phase 1: Send request → receive response head (TTFB)
            // ═══════════════════════════════════════════════════════
            // Backlog line 246: stamp the timing slot AFTER the request is
            // fully built and signed so build+sign overhead is excluded from
            // blocked/waiting, and total = Σphases actually holds.
            let hop_start = std::time::Instant::now();
            crate::subtimings::stamp_request_start(&slot, hop_start);

            // The response head (status line + headers) is received when this
            // resolves. The measured "waiting" time includes everything up to
            // this point: blocked + DNS + TCP connect + TLS handshake + sending +
            // server processing.
            let waiting_start = std::time::Instant::now();
            let mut response =
                crate::subtimings::TimedRequest::new(client.execute(built_request), slot.clone())
                    .await
                    .map_err(|e| http_request_error(&e))?;
            let mut waiting_duration = waiting_start.elapsed();

            // HTTP Digest (RFC 7616) is challenge-response: the first request goes
            // out unauthenticated, and on a 401 with a `WWW-Authenticate: Digest`
            // header we compute the Authorization value and retry once. The
            // retried response replaces the 401 for all downstream processing.
            if response.status().as_u16() == 401 {
                if let Some(signer) = signer {
                    // A server may send several `WWW-Authenticate` header LINES
                    // (one per scheme: `Basic realm=...` then `Digest realm=...`)
                    // — the old `.get()` read only the first line, so a Digest
                    // challenge on a later line was never seen (backlog line 176).
                    let www: Vec<String> = response
                        .headers()
                        .get_all(reqwest::header::WWW_AUTHENTICATE)
                        .iter()
                        .filter_map(|v| v.to_str().ok())
                        .map(str::to_string)
                        .collect();
                    let www = www.join(", ");
                    if !www.is_empty() {
                        if let Some(mut retry) = retry_request {
                            if let Some(auth_value) = signer.challenge_response(&www, &retry) {
                                retry.headers_mut().insert(
                                    reqwest::header::AUTHORIZATION,
                                    auth_value.parse().map_err(|_| {
                                        TropelError::Http(
                                            "Invalid digest Authorization header value".into(),
                                        )
                                    })?,
                                );
                                let retry_start = std::time::Instant::now();
                                response = crate::subtimings::TimedRequest::new(
                                    client.execute(retry),
                                    slot.clone(),
                                )
                                .await
                                .map_err(|e| http_request_error(&e))?;
                                // P2 line 177: accumulate retry time instead
                                // of replacing. The old code overwrote
                                // waiting_duration, making the 401 round-trip
                                // invisible in the phase breakdown.
                                waiting_duration += retry_start.elapsed();
                                tracing::debug!(
                                    "Digest auth: retried after 401 challenge (status now {})",
                                    response.status().as_u16()
                                );
                            }
                        }
                    }
                }
            }

            let status_code = response.status().as_u16();
            let status_text = response
                .status()
                .canonical_reason()
                .unwrap_or("Unknown")
                .to_string();
            // Line 368/369: capture the actual protocol version from the response
            // instead of hardcoding "HTTP/1.1" in the JS shim. reqwest exposes
            // the negotiated version after the TLS/ALPN handshake.
            // reqwest::Version is a re-export of http::Version; it has no
            // Display impl, but Debug outputs e.g. "Http(2, 0)" for HTTP/2.
            let protocol = format_http_version(response.version());

            // Collect response headers — canonicalized to Go's MIME form
            // (Content-Type, X-Request-Id) because reqwest's HeaderName is
            // always lowercase; k6/Postman scripts index headers by their
            // canonical spelling and every doc example would otherwise see
            // undefined. `raw_headers` keeps EVERY line in arrival order
            // (duplicate names preserved) for lossless consumers.
            let mut headers: HashMap<String, String> = HashMap::new();
            let mut raw_headers: Vec<(String, String)> = Vec::new();
            for (k, v) in response.headers().iter() {
                let name = canonical_header_name(k.as_str());
                let value = v.to_str().unwrap_or("").to_string();
                // TR-231: multi-valued headers are ", "-joined into one string
                // (k6 parity — Go's Header.Get joins duplicate values that way,
                // and Response.headers is the script-facing map). The old
                // `insert` was last-write-wins, so a `Set-Cookie` or repeated
                // `X-Foo` response lost all but the final value. raw_headers
                // still keeps EVERY line for lossless consumers.
                match headers.get_mut(&name) {
                    Some(existing) => {
                        existing.push_str(", ");
                        existing.push_str(&value);
                    }
                    None => {
                        headers.insert(name.clone(), value.clone());
                    }
                }
                raw_headers.push((name, value));
            }

            // Parse Set-Cookie headers into structured cookies so scripts can
            // read `res.cookies` (pm.response.cookies / k6 res.cookies). The
            // header may appear multiple times (one per cookie); a HashMap would
            // collapse them, so we walk `get_all` on the raw header map.
            let cookies: Vec<Cookie> = response
                .headers()
                .get_all(reqwest::header::SET_COOKIE)
                .iter()
                .filter_map(|v| parse_set_cookie(v.to_str().ok()?))
                .collect();

            // Feed this hop's Set-Cookie headers into the per-VU jar. Runs for
            // EVERY hop (including redirect hops), so a session cookie set by
            // an intermediate redirect is available to the next hop.
            if let Some(jar) = jar {
                for v in response.headers().get_all(reqwest::header::SET_COOKIE) {
                    if let Ok(s) = v.to_str() {
                        jar.add_cookie_str(s, &hop_url);
                    }
                }
            }

            // ── Redirect hop? ──
            // k6 parity: every redirect hop is its own request. When the response
            // is a 3xx with a Location header (and hops remain), capture it as a
            // hop response, resolve the next URL, rewrite method/body per RFC
            // 7231, and loop to send the next hop.
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let is_redirect = matches!(status_code, 301 | 302 | 303 | 307 | 308);
            // P1 line 155: when max_redirects is exceeded, return an error
            // instead of treating the 3xx as success. k6 errors with
            // "stopped after N redirects" — an infinite redirect loop would
            // otherwise report a 100% green run (the default
            // expected_statuses [200-399] matches 3xx).
            if manual_follow && is_redirect && redirects.len() >= max_hops {
                return Err(TropelError::Http(format!(
                    "stopped after {} redirect{} (max {})",
                    redirects.len(),
                    if redirects.len() == 1 { "" } else { "s" },
                    max_hops,
                )));
            }
            if manual_follow && redirects.len() < max_hops && is_redirect {
                if let Some(location) = location {
                    // Capture the hop as its own response (own duration). The
                    // body is the usually-tiny redirect body — drain it so the
                    // connection returns to the pool. The response-size cap
                    // applies here too, so a giant redirect body can't bypass
                    // it (the final body has the same ceiling).
                    let mut hop_body: Vec<u8> = Vec::new();
                    while let Some(chunk) = response.chunk().await.map_err(|e| {
                        TropelError::Http(format!("Failed to read redirect body: {}", e))
                    })? {
                        if let Some(cap) = self.config.max_response_bytes {
                            if hop_body.len() as u64 + chunk.len() as u64 > cap {
                                return Err(TropelError::Http(format!(
                                    "redirect body exceeds the {cap} byte limit"
                                )));
                            }
                        }
                        hop_body.extend_from_slice(&chunk);
                    }
                    let hop_total = hop_start.elapsed();
                    let hop_phases = crate::subtimings::take_slot(&slot);
                    // TR-202: same k6 tracer.go assembly as the final response —
                    // the hop's `sending` is real (timed body wrapper), and the
                    // waiting guard + reused-connection basis are identical.
                    let hop_timings = crate::subtimings::k6_done(
                        &hop_phases,
                        waiting_duration,
                        Duration::ZERO,
                        hop_total,
                    );

                    // `size` counts the drained hop body bytes so data_received
                    // per hop matches the wire (k6 counts per-request
                    // data_received).
                    let hop_size = hop_body.len() as u64;
                    redirects.push(HttpResponse {
                        url: current_url.clone(),
                        status_code,
                        status_text,
                        protocol: format_http_version(response.version()),
                        headers,
                        raw_headers,
                        body: hop_body,
                        text_cache: std::sync::OnceLock::new(),
                        json_cache: std::sync::OnceLock::new(),
                        response_time: hop_total,
                        timings: Some(hop_timings),
                        cookies,
                        size: hop_size,
                        request_body_size: 0,
                        redirects: Vec::new(),
                    });

                    // Resolve the Location header against the current URL.
                    let base = &hop_url;
                    let next = base.join(&location).map_err(|e| {
                        TropelError::Http(format!(
                            "Invalid redirect Location '{}': {}",
                            location, e
                        ))
                    })?;

                    // Cross-origin redirect → drop credentials for the next hop.
                    // Reuse `base` (already parsed above) instead of parsing
                    // the same URL a second time.
                    let same_origin = base.scheme() == next.scheme()
                        && base.host_str() == next.host_str()
                        && base.port_or_known_default() == next.port_or_known_default();
                    // Backlog line 246: the latch must be RESET on same-origin
                    // hops so a cross→same→cross redirect chain re-applies
                    // credentials on the middle hop instead of permanently
                    // stripping them after the first cross-origin hop.
                    strip_sensitive = !same_origin;

                    // RFC 7231 method rewrite (matches reqwest/k6):
                    //   303 → GET (drop body), except HEAD stays HEAD
                    //   301/302 → GET only for POST (drop body)
                    //   307/308 → keep method and body
                    match status_code {
                        303 if current_method != Method::HEAD => {
                            current_method = Method::GET;
                            current_body = None;
                        }
                        301 | 302 if current_method == Method::POST => {
                            current_method = Method::GET;
                            current_body = None;
                        }
                        _ => {}
                    }

                    tracing::debug!("Redirect {}: {} -> {}", status_code, current_url, next);
                    current_url = next.to_string();
                    hop_index += 1;
                    continue;
                }
            }

            // ═══════════════════════════════════════════════════════
            // Phase 2: Receive response body
            // ═══════════════════════════════════════════════════════
            // The body is drained-but-not-stored when the GLOBAL
            // discard_response_bodies flag is set OR the per-request k6
            // `responseType: "none"` is requested — scripts see an empty body,
            // but the bytes are still read so the pooled connection survives.
            let receiving_start = std::time::Instant::now();
            let discard = self.discard_bodies
                || request.response_type == tropel_sdk::types::ResponseType::None;
            // When the body is discarded (global `discardResponseBodies` or the
            // per-request k6 `responseType: "none"`), we must STILL read the body
            // off the wire so reqwest can return the connection to the pool.
            // Dropping the `Response` unread closes the socket — every request
            // then opens a fresh TCP connection, the exact opposite of the
            // pooling these flags are meant to preserve. We drain the body and
            // throw the bytes away; the drained byte count still feeds
            // `size`/`data_received` so accounting matches the wire.
            //
            // Behavior notes (intended): with discard, `http_req_receiving` and
            // `data_received` now reflect the real drain time / wire bytes instead
            // of ~0 — k6 still downloads the body and only skips storing it. And
            // a server that streams forever now fails at the request timeout
            // (chunk error) instead of silently succeeding with an empty body,
            // which is more correct.
            let (body_vec, size) = if discard {
                let mut drained: u64 = 0;
                while let Some(chunk) = response.chunk().await.map_err(|e| {
                    TropelError::Http(format!("Failed to drain response body: {}", e))
                })? {
                    drained += chunk.len() as u64;
                }
                (Vec::new(), drained)
            } else if let Some(cap) = self.config.max_response_bytes {
                // Stream with a hard ceiling instead of `response.bytes()`:
                // a runaway upstream is aborted mid-body once the cap would be
                // exceeded, keeping the proxy's memory use bounded.
                let mut body: Vec<u8> = Vec::new();
                while let Some(chunk) = response.chunk().await.map_err(|e| {
                    TropelError::Http(format!("Failed to read response body: {}", e))
                })? {
                    if body.len() as u64 + chunk.len() as u64 > cap {
                        return Err(TropelError::Http(format!(
                            "response body exceeds the {cap} byte limit"
                        )));
                    }
                    body.extend_from_slice(&chunk);
                }
                let len = body.len() as u64;
                (body, len)
            } else {
                let body = response
                    .bytes()
                    .await
                    .map_err(|e| TropelError::Http(format!("Failed to read response body: {}", e)))?
                    .to_vec();
                let len = body.len() as u64;
                (body, len)
            };
            let receiving_duration = receiving_start.elapsed();

            // The FINAL hop's own duration (k6 reports the last hop's time in its
            // final http_req_duration sample; the whole-chain wall time lives in
            // iteration_duration, so don't double-count the redirect hops here).
            let total_duration = hop_start.elapsed();

            // Build sub-timings from the real phases recorded by the
            // `dns_resolver` and `connector_layer` hooks (thread-local slot).
            // When the request reused a pooled keep-alive connection no connector
            // call happened, so the connect phases are ZERO — matching k6, which
            // also reports ~0 blocked/connecting for pooled connections.
            //
            // Note: `dns` is optional on purpose — for IP-literal hosts (e.g.
            // "127.0.0.1") reqwest's HttpConnector skips DNS resolution entirely,
            // so only the connect phases exist.
            let phases = crate::subtimings::take_slot(&slot);
            // TR-202: full k6 `tracer.go` `Done()` port — real `sending`
            // (measured by the timed body wrapper), the TLS-vs-plain sending
            // basis, the reused-connection stamp overwrite, and the
            // `gotFirstResponseByte > wroteRequest` waiting guard. `waiting`
            // excludes the request-write time, so `http_req_duration =
            // sending + waiting + receiving` no longer undercounts.
            let timings = crate::subtimings::k6_done(
                &phases,
                waiting_duration,
                receiving_duration,
                total_duration,
            );

            if self.http_debug {
                if self.http_debug_full {
                    let body_preview =
                        String::from_utf8_lossy(&body_vec[..body_vec.len().min(1024)]).into_owned();
                    let headers: Vec<String> = headers
                        .iter()
                        .map(|(k, v)| format!("{}: {}", k, v))
                        .collect();
                    tracing::info!(
                        "HTTP <<< {:?} {} -> {} headers={:?} body={:?} in {:.2?}",
                        request.method,
                        current_url,
                        status_code,
                        headers,
                        body_preview,
                        total_duration
                    );
                } else {
                    tracing::info!(
                        "HTTP <<< {:?} {} -> {} ({} bytes in {:.2?})",
                        request.method,
                        current_url,
                        status_code,
                        size,
                        total_duration
                    );
                }
            }

            let response = HttpResponse {
                url: current_url,
                status_code,
                status_text,
                protocol,
                headers,
                raw_headers,
                body: body_vec,
                text_cache: std::sync::OnceLock::new(),
                json_cache: std::sync::OnceLock::new(),
                response_time: total_duration,
                timings: Some(timings),
                cookies,
                size,
                request_body_size,
                // Every intermediate redirect hop, in order — callers emit one
                // http_req_* sample set per hop (k6 parity: a 302 chain counts
                // as hops + 1 requests, not just the final).
                redirects,
            };

            return Ok(response);
        } // end redirect-follow loop
    }
    /// Get an auth signer based on the auth config.
    ///
    /// Delegates to the single consolidated signer builder
    /// ([`tropel_auth::build_auth_signer`]) shared with the executor runner,
    /// so every auth type (Bearer, Basic, ApiKey, OAuth2, SigV4, OAuth1,
    /// Hawk, Digest) is supported in exactly one place.
    ///
    /// TR-409: unsupported schemes are reported as `Err(...)` rather than
    /// `Ok(None)` so the caller can surface `unsupported` instead of sending
    /// the request unauthenticated (TR-004 shape).
    pub fn get_signer(&self, auth: &AuthConfig) -> Result<Option<Box<dyn AuthSigner>>> {
        tropel_auth::build_auth_signer(auth)
    }
}

/// Per-VU HTTP client: shares the connection-pooled [`HttpClient`] (clones of
/// a `reqwest::Client` share the underlying pool, which is the point — one
/// pool per run) but owns its OWN cookie jar, so cookies are isolated per VU
/// (k6 semantics: each VU has its own jar). The wrapper is the sole cookie
/// authority — the inner clients are built WITHOUT a built-in store (see
/// [`HttpClient::build_client`]), so nothing can leak a global jar across VUs.
///
/// The jar is `Arc`-wrapped so the runner and the PM bridge can SHARE one
/// jar per VU (backlog line 159) — construct one client, then derive the
/// other with [`VuCookieClient::clone_with_shared_jar`].
pub struct VuCookieClient {
    inner: HttpClient,
    jar: Arc<reqwest::cookie::Jar>,
    /// Backlog line 244: cache signers per-VU so stateful signers (Digest)
    /// persist their session maps across requests. Uses `Box::leak` to get
    /// a `&'static dyn AuthSigner` that the VU's execute loop can borrow.
    /// The cache key is a serialized form of the auth config.
    signer_cache: Mutex<HashMap<String, &'static dyn AuthSigner>>,
}

impl VuCookieClient {
    /// Wrap a shared (connection-pooled) client with a fresh, empty jar.
    pub fn new(inner: HttpClient) -> Self {
        Self {
            inner,
            jar: Arc::new(reqwest::cookie::Jar::default()),
            signer_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Clone the shared inner client while REUSING this client's jar.
    pub fn clone_with_shared_jar(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            jar: self.jar.clone(),
            signer_cache: Mutex::new(HashMap::new()),
        }
    }

    /// The shared inner client (pool, TLS, RPS limiter, redirect policy).
    pub fn inner(&self) -> &HttpClient {
        &self.inner
    }

    /// Get a cached signer for this auth config. Digest signers are cached
    /// so their session maps persist across requests within the same VU.
    /// Stateless signers (Bearer, Basic, ApiKey) are also cached for
    /// consistency, though the benefit is marginal.
    ///
    /// TR-409: propagates `Err(unsupported)` from the builder so the VU loop
    /// can emit a transport-error sample rather than sending an unsigned
    /// request.
    pub fn get_signer_ref(
        &self,
        auth: &AuthConfig,
    ) -> Result<Option<&'static dyn AuthSigner>> {
        let key = auth_cache_key(auth);
        let mut cache = self.signer_cache.lock().unwrap();
        if let Some(&signer) = cache.get(&key) {
            return Ok(Some(signer));
        }
        let boxed = match self.inner.get_signer(auth)? {
            Some(b) => b,
            None => return Ok(None),
        };
        let static_ref: &'static dyn AuthSigner = Box::leak(boxed);
        cache.insert(key, static_ref);
        Ok(Some(static_ref))
    }

    /// Auth-signer builder (passthrough to the shared client, returns owned Box).
    pub fn get_signer(
        &self,
        auth: &AuthConfig,
    ) -> Result<Option<Box<dyn AuthSigner>>> {
        self.inner.get_signer(auth)
    }

    /// Execute a request through the shared client, applying this VU's cookie
    /// jar: stored cookies are injected per hop and every `Set-Cookie` header
    /// on each hop response is stored back into the jar.
    pub async fn execute(
        &self,
        request: &Request,
        signer: Option<&dyn AuthSigner>,
    ) -> Result<HttpResponse> {
        self.inner
            .execute_with_jar(request, signer, Some(self.jar.as_ref()))
            .await
    }
}

/// Cache key for an auth config — used by [`VuCookieClient::get_signer_ref`]
/// to persist stateful signers (Digest) across requests within a VU.
fn auth_cache_key(auth: &AuthConfig) -> String {
    match auth {
        AuthConfig::Bearer { token } => format!("bearer:{token}"),
        AuthConfig::Basic { username, password } => format!("basic:{username}:{password}"),
        AuthConfig::ApiKey {
            key,
            value,
            location,
        } => format!("apikey:{key}:{value}:{location:?}"),
        AuthConfig::Digest { username, password } => format!("digest:{username}:{password}"),
        AuthConfig::NoAuth => "noauth".to_string(),
        AuthConfig::OAuth2 { access_token, .. } => format!("oauth2:{access_token}"),
        AuthConfig::AwsSigV4 { access_key, .. } => format!("sigv4:{access_key}"),
        AuthConfig::OAuth1 {
            consumer_key,
            signature_method,
            ..
        } => format!(
            "oauth1:{consumer_key}:{}",
            signature_method.as_deref().unwrap_or("HMAC-SHA1")
        ),
        AuthConfig::Hawk { auth_id, .. } => format!("hawk:{auth_id}"),
        AuthConfig::Ntlm { username, .. } => format!("ntlm:{}", username.as_deref().unwrap_or("")),
        AuthConfig::Wsse { username, .. } => format!("wsse:{}", username.as_deref().unwrap_or("")),
        AuthConfig::Jwt { token, .. } => format!("jwt:{}", token.as_deref().unwrap_or("")),
        AuthConfig::AkamaiEdgeGrid { client_token, .. } => {
            format!("akamai:{}", client_token.as_deref().unwrap_or(""))
        }
    }
}

/// k6 `blacklistIPs` enforcement for IP-literal hosts.
///
/// The DNS resolver filters hostnames (every resolved address is checked),
/// but an IP-literal URL (`http://127.0.0.1:8080`, `http://[::1]/`) never
/// triggers a lookup — reqwest hands the literal straight to the connector.
/// This rejects a literal that falls inside any blacklisted CIDR BEFORE any
/// connection attempt. Hostnames pass through untouched (the resolver owns
/// them); a URL whose host does not parse as an IP is not our concern.
fn check_literal_blacklist(blacklist: &[IpCidr], url: &str) -> Result<()> {
    if blacklist.is_empty() {
        return Ok(());
    }
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| TropelError::Http(format!("Invalid request URL '{}': {}", url, e)))?;
    let Some(host) = parsed.host_str() else {
        return Ok(());
    };
    // The url crate serializes IPv6 hosts WITH brackets and normalizes
    // v4-mapped forms to hex (`http://[::ffff:10.1.2.3]` → `[::ffff:a01:203]`),
    // so strip the brackets before parsing as an IP.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let Ok(ip) = host.parse::<std::net::IpAddr>() else {
        // Hostname → the DNS resolver applies the blacklist.
        return Ok(());
    };
    if blacklist.iter().any(|c| c.contains(ip)) {
        return Err(TropelError::Http(format!(
            "request to blacklisted IP literal '{}' (blacklistIPs)",
            host
        )));
    }
    Ok(())
}

/// HTTP response data (mirrors `tropel_sdk::Response` but from reqwest).
/// Body text and JSON are lazily decoded ONCE and memoized — see
/// `body_text()` / `body_json()`.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// The URL that produced THIS response. For a redirect chain, each hop
    /// carries its own URL; the final response carries the final URL (what
    /// scripts see via pm.response / res).
    pub url: String,
    pub status_code: u16,
    pub status_text: String,
    /// Actual protocol version from the response (e.g. "HTTP/1.1", "HTTP/2").
    /// Previously hardcoded to "HTTP/1.1" in the JS shim (Line 368/369).
    pub protocol: String,
    pub headers: HashMap<String, String>,
    /// Every response header in ARRIVAL ORDER with duplicate names preserved
    /// (unlike the `headers` map, where the last line wins). Consumers that
    /// need lossless forwarding (proxy relays) use this; script-facing APIs
    /// keep using the map. Entries carry canonicalized names
    /// ([`canonical_header_name`]).
    pub raw_headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Memoized UTF-8 decode of `body` (see `body_text()`).
    pub text_cache: std::sync::OnceLock<Option<String>>,
    /// Memoized JSON parse of `body` (see `body_json()`).
    pub json_cache: std::sync::OnceLock<Option<serde_json::Value>>,
    pub response_time: Duration,
    pub timings: Option<Timings>,
    pub cookies: Vec<Cookie>,
    pub size: u64,
    /// Number of bytes sent in the request body (for data_sent tracking).
    pub request_body_size: u64,
    /// Intermediate redirect hops, in order, each captured as its own
    /// request (k6 parity). Empty when the request did not redirect (or
    /// `follow_redirects` / `--no-redirects` disabled following). Callers
    /// emit one http_req_* sample set PER hop plus the final response.
    pub redirects: Vec<HttpResponse>,
}

impl From<&HttpResponse> for tropel_sdk::types::Response {
    fn from(resp: &HttpResponse) -> Self {
        tropel_sdk::types::Response {
            url: resp.url.clone(),
            status_code: resp.status_code,
            status_text: resp.status_text.clone(),
            protocol: resp.protocol.clone(),
            headers: resp.headers.clone(),
            body: resp.body.clone(),
            text_cache: std::sync::OnceLock::new(),
            json_cache: std::sync::OnceLock::new(),
            response_time: resp.response_time,
            timings: resp.timings.clone(),
            cookies: resp.cookies.clone(),
            size: resp.size,
            request_body_size: resp.request_body_size,
            // Recursively convert the redirect chain so DriverHttpClient / k6
            // driver / pm.response all see per-hop requests (k6 parity).
            redirects: resp.redirects.iter().map(Response::from).collect(),
        }
    }
}

/// By-value conversion: moves all fields instead of cloning them.
/// Backlog line 312: the hot path (vu_loop.rs:485) drops HttpResponse
/// immediately after converting, so all the `.clone()`s in the by-ref
/// impl are pure waste — 16 allocations and two full body copies per
/// request. Use this instead where ownership is transferred.
impl From<HttpResponse> for tropel_sdk::types::Response {
    fn from(resp: HttpResponse) -> Self {
        tropel_sdk::types::Response {
            url: resp.url,
            status_code: resp.status_code,
            status_text: resp.status_text,
            protocol: resp.protocol,
            headers: resp.headers,
            body: resp.body,
            text_cache: std::sync::OnceLock::new(),
            json_cache: std::sync::OnceLock::new(),
            response_time: resp.response_time,
            timings: resp.timings,
            cookies: resp.cookies,
            size: resp.size,
            request_body_size: resp.request_body_size,
            redirects: resp.redirects.into_iter().map(Response::from).collect(),
        }
    }
}

impl HttpResponse {
    /// Decode the body as UTF-8 text (lazy — decodes once, then memoized).
    ///
    /// Postman parity (backlog line 171): an EMPTY body yields `Some("")`
    /// (Postman's `pm.response.text()` returns `''`, not `undefined`), and a
    /// non-UTF-8 body is decoded LOSSILY instead of becoming `null` — so
    /// `res.body.includes(...)` on a binary/odd-encoding response doesn't
    /// throw `undefined` method errors.
    pub fn body_text(&self) -> Option<String> {
        self.text_cache
            .get_or_init(|| {
                if self.body.is_empty() {
                    Some(String::new())
                } else {
                    Some(String::from_utf8_lossy(&self.body).into_owned())
                }
            })
            .clone()
    }

    /// Parse the body as JSON (lazy — parses once, then memoized).
    /// TR-312: `serde_json::from_slice` borrows `&[u8]` without the `body.clone()`
    /// that `simd-json`'s `&mut [u8]` API required — one full body copy per call
    /// eliminated (6→2 floor). The simd speedup is ~2-4× but the clone dominated
    /// at 1 MB bodies, so net is faster.
    pub fn body_json(&self) -> Option<serde_json::Value> {
        self.json_cache
            .get_or_init(|| {
                if self.body.is_empty() {
                    return None;
                }
                serde_json::from_slice(&self.body).ok()
            })
            .clone()
    }
}

/// Parse a single `Set-Cookie` header value into a structured [`Cookie`].
///
/// Handles the standard `name=value; Attr=Val; Flag` grammar — name/value are
/// the bare pair, then optional `Domain`, `Path`, `Expires`, `SameSite`
/// attributes plus the boolean `HttpOnly` / `Secure` flags. Unknown attributes
/// are ignored. Returns `None` when the header has no `name=value` pair.
fn parse_set_cookie(header: &str) -> Option<Cookie> {
    let mut parts = header.split(';');
    let pair = parts.next()?.trim();
    let (name, value) = pair.split_once('=')?;

    let mut cookie = Cookie {
        name: name.trim().to_string(),
        value: value.trim().to_string(),
        domain: None,
        path: None,
        http_only: None,
        secure: None,
        same_site: None,
        expires: None,
        max_age: None,
    };

    for attr in parts {
        let attr = attr.trim();
        if attr.is_empty() {
            continue;
        }
        // Case-insensitive attribute NAMES (Set-Cookie attrs are
        // case-insensitive per RFC 6265) — but the VALUE keeps its original
        // case (e.g. SameSite=Lax must not become "lax"). Split the raw attr
        // on '=' and lowercase only the key for matching.
        match attr.split_once('=') {
            Some((key, val)) => match key.trim().to_ascii_lowercase().as_str() {
                "domain" => cookie.domain = Some(val.trim().trim_matches('"').to_string()),
                "path" => cookie.path = Some(val.trim().trim_matches('"').to_string()),
                "expires" => cookie.expires = Some(val.trim().trim_matches('"').to_string()),
                "samesite" => cookie.same_site = Some(val.trim().trim_matches('"').to_string()),
                _ => {}
            },
            None => match attr.to_ascii_lowercase().as_str() {
                "httponly" => cookie.http_only = Some(true),
                "secure" => cookie.secure = Some(true),
                _ => {}
            },
        }
    }
    Some(cookie)
}

/// Parse a `Cookie` header value into `(name, value)` pairs. Cookie values
/// may contain `=` but never `;` (the pair separator), so splitting on `;`
/// and then on the FIRST `=` is correct — the same split the jar merge uses
/// to apply k6's replace semantics per cookie name.
fn parse_cookie_header_value(value: &str) -> Vec<(String, String)> {
    value
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|pair| {
            let (name, val) = pair.split_once('=')?;
            Some((name.trim().to_string(), val.trim().to_string()))
        })
        .collect()
}

/// Parse a TLS version string ("1.2", "tls1.2", "1.3", ...) into a reqwest
/// TLS version. Returns None for unrecognized/empty values (builder defaults).
fn parse_tls_version(s: &Option<String>) -> Option<reqwest::tls::Version> {
    let v = s.as_deref()?.trim().to_ascii_lowercase();
    match v.as_str() {
        "1.0" | "tls1.0" | "tlsv1.0" | "tls1" => Some(reqwest::tls::Version::TLS_1_0),
        "1.1" | "tls1.1" | "tlsv1.1" => Some(reqwest::tls::Version::TLS_1_1),
        "1.2" | "tls1.2" | "tlsv1.2" | "tls12" => Some(reqwest::tls::Version::TLS_1_2),
        "1.3" | "tls1.3" | "tlsv1.3" | "tls13" => Some(reqwest::tls::Version::TLS_1_3),
        _ => None,
    }
}

/// Calculate the byte size of a request body.
/// Serialize a request body to its exact wire bytes — the SINGLE serializer
/// for both data_sent accounting and the reqwest body. Keeping one source of
/// truth guarantees `body_size` can never diverge from what's actually sent
/// (the old UrlEncoded branch used an un-encoded `k.len()+v.len()+1`
/// approximation that under-counted vs the serde_urlencoded wire bytes).
fn body_to_bytes(body: &Body) -> Vec<u8> {
    match body {
        Body::Raw(s) => s.as_bytes().to_vec(),
        Body::Json(val) => serde_json::to_string(val).unwrap_or_default().into_bytes(),
        Body::FormData(parts) => multipart_form_data_bytes(parts),
        Body::UrlEncoded(fields) => {
            // W2 #203: fields are an ordered Vec (declaration order,
            // duplicates preserved) — byte-stable by construction, so no
            // sort is needed (the old HashMap required the key sort for
            // run-to-run determinism, backlog line 143).
            serde_urlencoded::to_string(fields)
                .unwrap_or_default()
                .into_bytes()
        }
        Body::Binary(data) => data.clone(),
        Body::GraphQL { query, variables } => {
            // The same serializer the client sends, so data_sent accounting
            // can't diverge from the actual body.
            Body::graphql_json_string(query, variables).into_bytes()
        }
    }
}

/// Exact wire size of a request body (delegates to the single serializer).
pub fn body_size(body: &Body) -> usize {
    body_to_bytes(body).len()
}

fn multipart_form_data_bytes(parts: &[FormDataPart]) -> Vec<u8> {
    let mut body = Vec::new();

    // Backlog line 143: sort by name so the multipart framing is byte-stable
    // run-to-run (a HashMap iterates in nondeterministic RandomState order).
    // Line 198: file parts now carry (filename, mime, raw bytes) — a part
    // with data is emitted with `filename=` and a per-part Content-Type,
    // which every mainstream parser keys the file branch off of.
    let mut fields: Vec<&FormDataPart> = parts.iter().collect();
    fields.sort_by(|a, b| a.name.cmp(&b.name));

    for part in fields {
        body.extend_from_slice(format!("--{}\r\n", MULTIPART_BOUNDARY).as_bytes());
        if let Some(data) = &part.data {
            body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n",
                    escape_multipart_field_name(&part.name),
                    escape_multipart_field_name(part.filename.as_deref().unwrap_or("file"))
                )
                .as_bytes(),
            );
            body.extend_from_slice(
                format!(
                    "Content-Type: {}\r\n\r\n",
                    part.mime.as_deref().unwrap_or("application/octet-stream")
                )
                .as_bytes(),
            );
            body.extend_from_slice(data);
        } else {
            body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{}\"\r\n\r\n",
                    escape_multipart_field_name(&part.name)
                )
                .as_bytes(),
            );
            if let Some(value) = &part.value {
                body.extend_from_slice(value.as_bytes());
            }
        }
        body.extend_from_slice(b"\r\n");
    }

    body.extend_from_slice(format!("--{}--\r\n", MULTIPART_BOUNDARY).as_bytes());
    body
}

fn escape_multipart_field_name(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Build the reqwest body from the single wire serializer (used by tests;
/// the hot path in `execute` reuses `body_to_bytes` output directly).
#[cfg(test)]
fn body_to_reqwest(body: &Body) -> reqwest::Body {
    reqwest::Body::from(body_to_bytes(body))
}

#[cfg(test)]
mod multipart_tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn multipart_form_data_serializes_with_boundary() {
        let formdata = vec![
            FormDataPart {
                name: "field1".to_string(),
                value: Some("value1".to_string()),
                filename: None,
                mime: None,
                data: None,
            },
            FormDataPart {
                name: "field 2".to_string(),
                value: Some("two".to_string()),
                filename: None,
                mime: None,
                data: None,
            },
            // Line 198: a file part must carry filename= + per-part
            // Content-Type so parsers route it to the file branch.
            FormDataPart {
                name: "upload".to_string(),
                value: None,
                filename: Some("photo.png".to_string()),
                mime: Some("image/png".to_string()),
                data: Some(vec![0x89, 0x50, 0x4e, 0x47]),
            },
        ];

        let bytes = multipart_form_data_bytes(&formdata);
        // from_utf8_LOSSY: the PNG file part is raw binary (0x89 lead byte
        // is not valid UTF-8) — lossy keeps the ASCII framing inspectable
        // without panicking on the file bytes.
        let text = String::from_utf8_lossy(&bytes);

        assert!(text.contains("Content-Disposition: form-data; name=\"field1\""));
        assert!(text.contains("Content-Disposition: form-data; name=\"field 2\""));
        assert!(text.contains("value1"));
        assert!(text.contains("two"));
        assert!(text
            .contains("Content-Disposition: form-data; name=\"upload\"; filename=\"photo.png\""));
        assert!(text.contains("Content-Type: image/png"));
        // Raw-byte check: the PNG signature 0x89 0x50 0x4E 0x47 must be on
        // the wire verbatim (lossy text shows U+FFFD for the 0x89 lead
        // byte, so assert on the raw bytes, not the text).
        assert!(
            bytes.windows(4).any(|w| w == [0x89, 0x50, 0x4e, 0x47]),
            "PNG signature bytes must be present verbatim"
        );
        assert!(text.ends_with("------------------------tropel-boundary-7a2f24b9--\r\n"));
        assert_eq!(body_size(&Body::FormData(formdata)), bytes.len());
    }

    #[test]
    fn graphql_body_includes_variables() {
        // Regression: `body_to_reqwest` destructured `variables: _` and sent
        // ONLY the query — a GraphQL request with variables silently dropped
        // them (the server would error or return wrong results).
        let mut vars: HashMap<String, serde_json::Value> = HashMap::new();
        vars.insert("id".to_string(), serde_json::json!("42"));
        let body = Body::GraphQL {
            query: "query($id: ID!) { user(id: $id) { name } }".to_string(),
            variables: Some(vars),
        };
        let req_body = body_to_reqwest(&body);
        let bytes = req_body.as_bytes().expect("reqwest body is bytes");
        let json: serde_json::Value =
            serde_json::from_slice(bytes).expect("GraphQL wire body is valid JSON");
        assert_eq!(json["query"], "query($id: ID!) { user(id: $id) { name } }");
        assert_eq!(json["variables"]["id"], "42");
        // body_size must account for the variables too (exact, not the old
        // `query.len() + 50` approximation).
        assert_eq!(body_size(&body), bytes.len());
    }

    #[test]
    fn graphql_body_omits_empty_variables() {
        // No variables map → the wire JSON has NO "variables" key at all
        // (strict servers reject an empty `variables: {}`).
        let body = Body::GraphQL {
            query: "{ hello }".to_string(),
            variables: None,
        };
        let req_body = body_to_reqwest(&body);
        let bytes = req_body.as_bytes().expect("reqwest body is bytes");
        let json: serde_json::Value =
            serde_json::from_slice(bytes).expect("GraphQL wire body is valid JSON");
        assert_eq!(json["query"], "{ hello }");
        assert!(json.get("variables").is_none());
        assert_eq!(body_size(&body), bytes.len());
    }
}

/// Delegate to the canonical `tropel_sdk::parse_duration`.
pub(crate) fn parse_duration(s: &str) -> Result<Duration> {
    tropel_sdk::parse_duration(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A minimal HTTP server that speaks just enough to exercise the cookie
    /// jar:
    /// - `GET /set` → 200 with `Set-Cookie: sid=abc123; Path=/`
    /// - `GET /echo` → 200 with the request's `Cookie` header as the body
    ///   (or `none` when absent)
    ///
    /// Responses use `Connection: close`, so every request opens a fresh
    /// connection — no keep-alive bookkeeping in the test.
    fn spawn_cookie_test_server() -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                let mut buf = [0u8; 8192];
                let mut n = 0usize;
                // Read until the full head ("\r\n\r\n") is in the buffer — a
                // single read() may return a partial request if the kernel
                // fragments it, which would drop the Cookie header and make
                // the /echo assertion flaky.
                while n < buf.len() {
                    match std::io::Read::read(&mut stream, &mut buf[n..]) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            n += read;
                            if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                if n == 0 {
                    continue;
                }
                let head = String::from_utf8_lossy(&buf[..n]);
                let request_line = head.lines().next().unwrap_or_default();
                let cookie = head
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("cookie:"))
                    .and_then(|l| l.split_once(':'))
                    .map(|(_, v)| v.trim().to_string())
                    .unwrap_or_else(|| "none".to_string());
                let (status, body, extra) = if request_line.contains("/set") {
                    (
                        "200 OK",
                        "set".to_string(),
                        "Set-Cookie: sid=abc123; Path=/\r\n".to_string(),
                    )
                } else {
                    ("200 OK", cookie, String::new())
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = std::io::Write::write_all(&mut stream, resp.as_bytes());
            }
        });
        (addr, handle)
    }

    fn get_request(url: &str) -> Request {
        Request {
            url: url.to_string(),
            method: Method::GET,
            headers: Vec::new(),
            query_params: HashMap::new(),
            body: None,
            auth: None,
            certificate: None,
            follow_redirects: true,
            host: None,
            cookies: Vec::new(),
            timeout: None,
            response_type: ResponseType::Text,
        }
    }

    #[test]
    fn per_vu_cookie_jar_is_isolated_and_persistent() {
        // Regression (backlog V2 §2): every VU shared ONE reqwest client with
        // ONE built-in cookie store, so a cookie set by VU A was sent by VU B.
        // Each per-VU VuCookieClient (own jar over the shared pool client,
        // whose built-in store is disabled) must isolate AND persist cookies.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (addr, _server) = spawn_cookie_test_server();
            let shared = HttpClient::new(&HttpConfig::default()).unwrap();
            let base = format!("http://{addr}");

            let vu_a = VuCookieClient::new(shared.clone());
            // VU A plants a cookie...
            let set = vu_a
                .execute(&get_request(&format!("{base}/set")), None)
                .await
                .unwrap();
            assert_eq!(set.status_code, 200);
            assert_eq!(set.cookies.len(), 1);
            assert_eq!(set.cookies[0].name, "sid");
            assert_eq!(set.cookies[0].value, "abc123");
            // ...and its NEXT request carries it (persistence within the VU).
            let echo_a = vu_a
                .execute(&get_request(&format!("{base}/echo")), None)
                .await
                .unwrap();
            assert_eq!(echo_a.status_code, 200);
            assert_eq!(echo_a.body_text().unwrap(), "sid=abc123");

            // VU B shares the same pool client but owns a fresh jar: it must
            // NOT see VU A's cookie (isolation) — and the shared client's
            // disabled built-in store must not leak it either.
            let vu_b = VuCookieClient::new(shared.clone());
            let echo_b = vu_b
                .execute(&get_request(&format!("{base}/echo")), None)
                .await
                .unwrap();
            assert_eq!(echo_b.status_code, 200);
            assert_eq!(echo_b.body_text().unwrap(), "none");
        });
    }

    /// TR-230: k6 SetRequestCookies merge — `request.cookies` with
    /// `replace:false` (default) sends BOTH the jar cookie and the request
    /// cookie; `replace:true` suppresses the jar's same-name cookie.
    #[tokio::test(flavor = "current_thread")]
    async fn request_cookies_replace_false_sends_both_jar_and_request_cookie() {
        let (addr, _server) = spawn_cookie_test_server();
        let shared = HttpClient::new(&HttpConfig::default()).unwrap();
        let vu = VuCookieClient::new(shared);
        let base = format!("http://{addr}");

        // Plant a jar cookie.
        let set = vu
            .execute(&get_request(&format!("{base}/set")), None)
            .await
            .unwrap();
        assert_eq!(set.status_code, 200);

        // Request with a cookie that has replace:false (default) — the jar
        // cookie "sid=abc123" AND the request cookie "sid=s1" must both arrive.
        let mut req = get_request(&format!("{base}/echo"));
        req.cookies.push(tropel_sdk::RequestCookie {
            name: "sid".to_string(),
            value: "s1".to_string(),
            replace: false,
        });
        let echo = vu.execute(&req, None).await.unwrap();
        assert_eq!(echo.status_code, 200);
        let body = echo.body_text().unwrap();
        assert!(
            body.contains("sid=abc123"),
            "jar cookie must be present (replace:false), server saw: {body}"
        );
        assert!(
            body.contains("sid=s1"),
            "request cookie must be present (replace:false), server saw: {body}"
        );

        // Request with replace:true — the jar cookie "sid=abc123" is
        // suppressed, only the request cookie "sid=s1" arrives.
        let mut req2 = get_request(&format!("{base}/echo"));
        req2.cookies.push(tropel_sdk::RequestCookie {
            name: "sid".to_string(),
            value: "s2".to_string(),
            replace: true,
        });
        let echo2 = vu.execute(&req2, None).await.unwrap();
        let body2 = echo2.body_text().unwrap();
        assert!(
            !body2.contains("sid=abc123"),
            "jar cookie must be suppressed (replace:true), server saw: {body2}"
        );
        assert!(
            body2.contains("sid=s2"),
            "request cookie must be present (replace:true), server saw: {body2}"
        );
    }

    #[test]
    fn shared_jar_clone_lets_bridge_cookie_reach_runner_requests() {
        // bridge each constructed a FRESH `VuCookieClient::new`, giving every
        // VU two empty jars. The canonical Postman auth pattern — prerequest
        // `pm.sendRequest` → `/login` → `Set-Cookie` — landed the session
        // cookie in the BRIDGE jar, while collection requests went out
        // through the RUNNER jar with no session → 401 for the whole run.
        // Clients derived via `clone_with_shared_jar` must share ONE jar.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (addr, _server) = spawn_cookie_test_server();
            let shared = HttpClient::new(&HttpConfig::default()).unwrap();
            let base = format!("http://{addr}");

            // The runner client owns the VU's jar; the bridge is derived
            // from it (this is exactly what vu_loop.rs does now).
            let runner = VuCookieClient::new(shared.clone());
            let bridge = runner.clone_with_shared_jar();

            // Prerequest auth: pm.sendRequest → /login sets the session
            // cookie through the BRIDGE client.
            let set = bridge
                .execute(&get_request(&format!("{base}/set")), None)
                .await
                .unwrap();
            assert_eq!(set.status_code, 200);
            assert_eq!(set.cookies.len(), 1);
            assert_eq!(set.cookies[0].name, "sid");

            // The very next collection request must carry the session cookie
            // — before the fix, the runner's empty jar sent no session and
            // the server returned the auth endpoint's 401.
            let echo = runner
                .execute(&get_request(&format!("{base}/echo")), None)
                .await
                .unwrap();
            assert_eq!(echo.status_code, 200);
            assert_eq!(
                echo.body_text().unwrap(),
                "sid=abc123",
                "cookie set via the bridge must be visible to the runner's jar"
            );
        });
    }

    #[test]
    fn canonical_header_name_matches_go_mime_form() {
        // Regression (backlog line 139): reqwest lowercases HeaderName, so
        // response headers arrived as `content-type` and k6's canonical
        // `res.headers['Content-Type']` (and Postman's
        // `pm.response.header('Content-Type')`) returned undefined.
        assert_eq!(canonical_header_name("content-type"), "Content-Type");
        assert_eq!(canonical_header_name("x-request-id"), "X-Request-Id");
        assert_eq!(canonical_header_name("etag"), "Etag");
        assert_eq!(
            canonical_header_name("www-authenticate"),
            "Www-Authenticate"
        );
        // Already-canonical and single-word names are idempotent.
        assert_eq!(canonical_header_name("Content-Type"), "Content-Type");
        assert_eq!(canonical_header_name("location"), "Location");
        assert_eq!(canonical_header_name("set-cookie"), "Set-Cookie");
    }

    #[test]
    fn http_response_body_text_lossy_and_empty() {
        // Regression (backlog line 171) — same contract as the core
        // `Response::body_text()`: empty → Some("") (Postman `''`), and
        // non-UTF-8 decodes LOSSILY so `res.body.includes(...)` never sees
        // null/undefined. Fresh caches per response (OnceCell memoization
        // must not leak across the struct-update clones).
        fn resp_with(body: Vec<u8>) -> HttpResponse {
            HttpResponse {
                url: String::new(),
                status_code: 200,
                status_text: "OK".into(),
                protocol: "HTTP/1.1".into(),
                headers: Default::default(),
                raw_headers: Default::default(),
                body,
                text_cache: Default::default(),
                json_cache: Default::default(),
                response_time: Duration::ZERO,
                timings: None,
                cookies: Vec::new(),
                size: 0,
                request_body_size: 0,
                redirects: Vec::new(),
            }
        }
        assert_eq!(resp_with(Vec::new()).body_text(), Some(String::new()));
        assert_eq!(
            resp_with(vec![0xC3, 0x28, 0x41]).body_text(),
            Some("\u{FFFD}(A".to_string())
        );
        assert_eq!(
            resp_with(b"ok".to_vec()).body_text(),
            Some("ok".to_string())
        );
    }

    #[test]
    fn parse_set_cookie_extracts_name_value_and_attrs() {
        let c = parse_set_cookie(
            "session=abc123; Path=/; Domain=example.com; HttpOnly; Secure; SameSite=Lax; Expires=Wed, 21 Oct 2026 07:28:00 GMT; Unknown=x",
        )
        .unwrap();
        assert_eq!(c.name, "session");
        assert_eq!(c.value, "abc123");
        assert_eq!(c.path.as_deref(), Some("/"));
        assert_eq!(c.domain.as_deref(), Some("example.com"));
        assert_eq!(c.http_only, Some(true));
        assert_eq!(c.secure, Some(true));
        assert_eq!(c.same_site.as_deref(), Some("Lax"));
        assert!(c.expires.as_deref().unwrap().starts_with("Wed, 21 Oct"));
    }

    #[test]
    fn parse_set_cookie_case_insensitive_attrs_and_quoted_values() {
        let c = parse_set_cookie("id=7; PATH=\"/app\"; HTTPONLY; sAmEsItE=Strict").unwrap();
        assert_eq!(c.name, "id");
        assert_eq!(c.value, "7");
        assert_eq!(c.path.as_deref(), Some("/app"));
        assert_eq!(c.http_only, Some(true));
        assert_eq!(c.same_site.as_deref(), Some("Strict"));
    }

    #[test]
    fn parse_set_cookie_minimal_and_garbage() {
        // Minimal `name=value` only.
        let c = parse_set_cookie("token=x").unwrap();
        assert_eq!(c.name, "token");
        assert_eq!(c.value, "x");
        assert!(c.path.is_none() && c.domain.is_none() && c.http_only.is_none());
        // No `name=value` pair → None. (Note: a leading segment that LOOKS
        // like an attribute, e.g. "Path=/; HttpOnly", is parsed as the
        // cookie-pair name="Path" value="/" — RFC 6265 requires the first
        // segment to be the cookie-pair, so this is only reachable on
        // malformed headers; the None paths below are the truly invalid ones.)
        assert!(parse_set_cookie("garbage-without-equals").is_none());
        assert!(parse_set_cookie("").is_none());
    }

    #[test]
    fn body_size_counts_percent_encoding_and_multipart_framing() {
        // Regression (backlog line 90): the deleted `Body::encoded_len`
        // measured UrlEncoded/FormData as a RAW `k=v&k=v` concat — no
        // percent-encoding and no multipart framing — so the k6/WASM
        // drivers' `data_sent` undercounted the real wire bytes. The single
        // serializer `body_size` must count what is actually sent.
        //
        // "a&b" percent-encodes to "a%26b" — `&` (1 byte) becomes `%26`
        // (3 bytes) — so the encoded size is LARGER than the raw concat.
        let url = Body::UrlEncoded(vec![("k".to_string(), "a&b".to_string())]);
        let wire = serde_urlencoded::to_string(vec![("k".to_string(), "a&b".to_string())]).unwrap();
        assert_eq!(wire, "k=a%26b");
        // 5 raw bytes vs 7 encoded — the old function reported 5.
        assert_eq!("k=a&b".len(), 5);
        assert_eq!(body_size(&url), wire.len());
        assert_eq!(wire.len(), 7);

        // Multipart framing: the wire body includes boundaries and
        // Content-Disposition headers, far larger than `k=v&k=v`.
        let framed = Body::FormData(vec![FormDataPart {
            name: "a".to_string(),
            value: Some("b".to_string()),
            filename: None,
            mime: None,
            data: None,
        }]);
        assert!(
            body_size(&framed) > 3,
            "multipart framing must exceed the raw k=v size"
        );
    }

    #[test]
    fn wire_bytes_are_deterministic_across_repeated_serialization() {
        // Backlog line 143: query_params, UrlEncoded and FormData are HashMap
        // with RandomState — iteration order differs between processes AND
        // between runs, so the wire bytes (and any script signing them) were
        // nondeterministic. The serializers sort by key; serializing the same
        // logically-equal maps many times must produce identical bytes.
        use std::collections::HashMap;

        let mut qp = HashMap::new();
        qp.insert("zeta".to_string(), "9".to_string());
        qp.insert("alpha".to_string(), "1".to_string());
        qp.insert("mid".to_string(), "x".to_string());

        let urlenc = vec![
            ("a".to_string(), "2".to_string()),
            ("m".to_string(), "3".to_string()),
            ("z".to_string(), "1".to_string()),
        ];

        let form = vec![
            FormDataPart {
                name: "z".to_string(),
                value: Some("1".to_string()),
                filename: None,
                mime: None,
                data: None,
            },
            FormDataPart {
                name: "a".to_string(),
                value: Some("2".to_string()),
                filename: None,
                mime: None,
                data: None,
            },
            FormDataPart {
                name: "m".to_string(),
                value: Some("3".to_string()),
                filename: None,
                mime: None,
                data: None,
            },
        ];

        // UrlEncoded: wire bytes follow DECLARATION ORDER (W2 #203 — the
        // fields are an ordered Vec now, byte-stable by construction; the old
        // HashMap required the key sort for determinism, backlog line 143).
        let urlenc_bytes = body_to_bytes(&Body::UrlEncoded(urlenc.clone()));
        assert_eq!(
            String::from_utf8_lossy(&urlenc_bytes),
            "a=2&m=3&z=1",
            "UrlEncoded must serialize key-sorted"
        );

        // FormData: key-sorted framing.
        let form_bytes = body_to_bytes(&Body::FormData(form.clone()));
        let ftext = String::from_utf8_lossy(&form_bytes);
        let pos_a = ftext.find("name=\"a\"").unwrap();
        let pos_m = ftext.find("name=\"m\"").unwrap();
        let pos_z = ftext.find("name=\"z\"").unwrap();
        assert!(
            pos_a < pos_m && pos_m < pos_z,
            "FormData parts must be key-sorted: {ftext}"
        );

        // query_params: exercised in execute() via .query(&sorted); assert the
        // sort helper produces stable ordering here.
        let mut sorted: Vec<(String, String)> =
            qp.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        sorted.sort();
        let keys: Vec<&str> = sorted.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn test_parse_duration() {
        assert_eq!(
            super::parse_duration("500ms").unwrap(),
            Duration::from_millis(500)
        );
        assert_eq!(super::parse_duration("5s").unwrap(), Duration::from_secs(5));
        assert_eq!(
            super::parse_duration("1.5s").unwrap(),
            Duration::from_millis(1500)
        );
        assert_eq!(
            super::parse_duration("2m").unwrap(),
            Duration::from_secs(120)
        );
        assert_eq!(
            super::parse_duration("1h").unwrap(),
            Duration::from_secs(3600)
        );
    }

    #[test]
    fn test_form_urlencoding() {
        let result = serde_urlencoded::to_string(vec![
            ("key".to_string(), "value".to_string()),
            ("name".to_string(), "hello world".to_string()),
        ])
        .unwrap();
        assert_eq!(result, "key=value&name=hello+world");
    }

    #[test]
    fn blacklist_rejects_ip_literal_but_not_hostname() {
        let blacklist = parse_blacklist(&["127.0.0.1".to_string(), "10.0.0.0/8".to_string()]);
        // Literal host inside the blacklist → rejected before any connect.
        assert!(check_literal_blacklist(&blacklist, "http://127.0.0.1:8080/api").is_err());
        assert!(check_literal_blacklist(&blacklist, "http://10.1.2.3/x").is_err());
        // v4-mapped v6 literal is canonicalized before matching.
        assert!(check_literal_blacklist(&blacklist, "http://[::ffff:10.1.2.3]/x").is_err());
        // Hostname → DNS resolver owns the blacklist; literal outside → fine.
        assert!(check_literal_blacklist(&blacklist, "http://example.com/").is_ok());
        assert!(check_literal_blacklist(&blacklist, "http://8.8.8.8/").is_ok());
        // Invalid URL surfaces as an error (not a silent pass).
        assert!(check_literal_blacklist(&blacklist, "not a url").is_err());
    }

    // ── per-request client selection (TROPEL_TODO_V2: "client cert and
    //    follow_redirects are ignored — fixed at client build") ──

    #[test]
    fn no_redirect_twin_built_only_when_following_enabled() {
        // max_redirects > 0 → a Policy::none() twin exists to serve
        // `follow_redirects: false` requests (the redirect policy is baked
        // into the client at build time).
        let cfg = HttpConfig::default(); // max_redirects = 10
        let client = HttpClient::new(&cfg).unwrap();
        assert!(client.no_redirect.is_some());

        // max_redirects == 0 → the primary client already never follows, so
        // no twin is needed and both request shapes share `inner`.
        let no_redirect_cfg = HttpConfig {
            max_redirects: 0,
            ..Default::default()
        };
        let client = HttpClient::new(&no_redirect_cfg).unwrap();
        assert!(client.no_redirect.is_none());
    }

    #[test]
    fn select_client_no_error_for_plain_requests() {
        // Both follow shapes resolve to a client (no panics / no errors);
        // behavioral proof of which one is used lives in the async redirect
        // test below.
        let cfg = HttpConfig::default();
        let client = HttpClient::new(&cfg).unwrap();
        for follow in [true, false] {
            let req = Request {
                follow_redirects: follow,
                ..Default::default()
            };
            assert!(client.select_client(&req).is_ok(), "follow={}", follow);
        }
    }

    /// TR-303: `http2_connections` builds N independent reqwest clients and
    /// lane selection round-robins across them. The config field existed but
    /// was never wired into the client — a flag set but nothing read it.
    #[test]
    fn http2_connections_builds_lanes_and_round_robins() {
        let cfg = HttpConfig {
            http2_connections: 3,
            ..Default::default()
        };
        let client = HttpClient::new(&cfg).unwrap();
        assert_eq!(client.inner.len(), 3, "3 lanes must be built");
        assert_eq!(
            client.no_redirect.as_ref().unwrap().len(),
            3,
            "no_redirect twin must mirror the lanes"
        );

        // Round-robin: three picks must hit three distinct lanes, then wrap.
        // `pick_lane` returns a reference into the Vec, so the address
        // comparison is stable (distinct elements → distinct addresses).
        let a = client.pick_lane(&client.inner) as *const _ as usize;
        let b = client.pick_lane(&client.inner) as *const _ as usize;
        let c = client.pick_lane(&client.inner) as *const _ as usize;
        assert_ne!(a, b, "lane 0 != lane 1 (round-robin)");
        assert_ne!(b, c, "lane 1 != lane 2 (round-robin)");
        let d = client.pick_lane(&client.inner) as *const _ as usize;
        assert_eq!(d, a, "lane selection wraps around after N picks");

        // A second client (new VU) shares the cursor (Arc) — it continues the
        // rotation, not restarts it.
        let client2 = client.clone();
        let e = client2.pick_lane(&client2.inner) as *const _ as usize;
        assert_ne!(e, a, "shared cursor must not restart the rotation");
    }

    #[test]
    fn select_client_cert_missing_file_errors() {
        // Regression: Request.certificate was silently ignored at client
        // build. A missing cert file must now surface a Config error instead
        // of proceeding without the identity.
        let cfg = HttpConfig::default();
        let client = HttpClient::new(&cfg).unwrap();
        let cert = CertificateConfig {
            cert: Some("missing.pem".to_string()),
            key: Some("missing.key".to_string()),
            passphrase: None,
        };
        // Both attempts fail on the missing files (proving the cert path IS
        // exercised) rather than being silently ignored.
        let follow_req = Request {
            certificate: Some(cert.clone()),
            follow_redirects: true,
            ..Default::default()
        };
        let no_follow_req = Request {
            certificate: Some(cert),
            follow_redirects: false,
            ..Default::default()
        };
        assert!(client.select_client(&follow_req).is_err());
        assert!(client.select_client(&no_follow_req).is_err());
    }

    #[test]
    fn select_client_certificate_requires_both_paths() {
        let cfg = HttpConfig::default();
        let client = HttpClient::new(&cfg).unwrap();
        let req = Request {
            certificate: Some(CertificateConfig {
                cert: Some("cert.pem".to_string()),
                key: None,
                passphrase: None,
            }),
            ..Default::default()
        };
        let err = client.select_client(&req).unwrap_err();
        assert!(format!("{}", err).contains("key path"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn client_level_request_timeout_bounds_hung_server() {
        // Backlog P1: the GLOBAL HttpConfig.request_timeout must bound a
        // server that accepts but never responds — not just k6's per-request
        // params.timeout. Isolation test at the client level: build with a
        // 300ms request_timeout and hit a hung server; the request must error
        // within seconds, not hang.
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                });
            }
        });

        let cfg = HttpConfig {
            request_timeout: Some("300ms".to_string()),
            ..Default::default()
        };
        let client = HttpClient::new(&cfg).unwrap();
        let req = Request {
            url: format!("http://{addr}/hi"),
            method: Method::GET,
            response_type: tropel_sdk::types::ResponseType::None,
            ..Default::default()
        };
        let start = std::time::Instant::now();
        let result = client.execute(&req, None).await;
        let elapsed = start.elapsed();
        assert!(
            result.is_err(),
            "hung request must time out at the client level, got {:?}",
            result
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "client-level timeout fired, took {elapsed:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn follow_redirects_false_returns_redirect_not_followed() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Tiny redirect server: /start → 302 → /final; /final → 200.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let req = String::from_utf8_lossy(&buf);
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let resp = if path == "/start" {
                        "HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                    } else {
                        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                            .to_string()
                    };
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        let cfg = HttpConfig::default();
        let client = HttpClient::new(&cfg).unwrap();

        // follow_redirects: true (default) → redirect is followed → 200.
        let follow_req = Request {
            url: format!("http://{}/start", addr),
            method: Method::GET,
            follow_redirects: true,
            ..Default::default()
        };
        let resp = client.execute(&follow_req, None).await.unwrap();
        assert_eq!(resp.status_code, 200, "redirect should be followed");

        // follow_redirects: false → the 302 is returned to the caller.
        let no_follow_req = Request {
            url: format!("http://{}/start", addr),
            method: Method::GET,
            follow_redirects: false,
            ..Default::default()
        };
        let resp = client.execute(&no_follow_req, None).await.unwrap();
        assert_eq!(resp.status_code, 302, "redirect must NOT be followed");

        server.abort();
    }

    /// Minimal signer: sets a fixed Authorization header, like a Bearer
    /// token signer. Lets the redirect tests observe whether the signed
    /// header survives to hop 1+.
    struct StaticSigner;

    impl AuthSigner for StaticSigner {
        fn name(&self) -> &str {
            "static"
        }

        fn sign(&self, request: &mut reqwest::Request) -> Result<()> {
            request.headers_mut().insert(
                reqwest::header::AUTHORIZATION,
                "Bearer s3cret".parse().unwrap(),
            );
            Ok(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn same_origin_redirect_keeps_signed_authorization() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // P0 regression: hop-0's request was signed in place on the built
        // reqwest::Request, but hop 1+ rebuilt from request.headers (never
        // signed), so the Authorization died with hop 0 and any same-origin
        // authenticated redirect 401'd. Server: /start → 302 → /final;
        // /final echoes whether it saw the Authorization header.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let req = String::from_utf8_lossy(&buf);
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let resp = if path == "/start" {
                        "HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                    } else if path == "/final" {
                        let saw_auth = req
                            .lines()
                            .any(|l| l.to_ascii_lowercase().starts_with("authorization:"));
                        let body = if saw_auth { "AUTH=YES" } else { "AUTH=NO" };
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                    } else {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    };
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        let cfg = HttpConfig::default();
        let client = HttpClient::new(&cfg).unwrap();
        let signer = StaticSigner;
        let req = Request {
            url: format!("http://{}/start", addr),
            method: Method::GET,
            follow_redirects: true,
            ..Default::default()
        };
        let resp = client.execute(&req, Some(&signer)).await.unwrap();
        assert_eq!(resp.status_code, 200);
        assert_eq!(
            String::from_utf8_lossy(&resp.body),
            "AUTH=YES",
            "signed Authorization must be forwarded on a same-origin redirect hop"
        );
        // The 302 hop must also be captured as its own response (k6 parity).
        assert_eq!(resp.redirects.len(), 1);
        assert_eq!(resp.redirects[0].status_code, 302);

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn redirect_hops_do_not_reappend_original_query() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // P1 regression: the original request's query_params were re-appended
        // on EVERY redirect hop — /x?page=2 → 302 /y?token=z fetched
        // /y?token=z&page=2. The redirect target's own query must win; the
        // original query belongs to hop 0 only (k6/reqwest behavior).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let req = String::from_utf8_lossy(&buf);
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let resp = if path == "/start?page=2" {
                        "HTTP/1.1 302 Found\r\nLocation: /final?token=z\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                    } else if path == "/final?token=z" {
                        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                            .to_string()
                    } else {
                        // Any OTHER path/query means the original query leaked
                        // onto the redirect hop — report it in the body so
                        // the test can assert the exact request target.
                        let body = format!("GOT:{}", path);
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                    };
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        let cfg = HttpConfig::default();
        let client = HttpClient::new(&cfg).unwrap();

        // Redirect case: URL carries its own query; query_params empty. The
        // final hop must be exactly /final?token=z (no re-appended page=2).
        let req = Request {
            url: format!("http://{}/start?page=2", addr),
            method: Method::GET,
            follow_redirects: true,
            ..Default::default()
        };
        let resp = client.execute(&req, None).await.unwrap();
        assert_eq!(
            resp.status_code,
            200,
            "final hop should be served; got body: {}",
            String::from_utf8_lossy(&resp.body)
        );
        assert_eq!(
            String::from_utf8_lossy(&resp.body),
            "ok",
            "original query must NOT be re-appended to the redirect target; got: {}",
            String::from_utf8_lossy(&resp.body)
        );
        assert_eq!(resp.redirects.len(), 1);
        assert_eq!(resp.redirects[0].status_code, 302);

        // Preserved hop-0 behavior: a populated query_params (URL without its
        // own query) must STILL be appended exactly once on hop 0 — the gate
        // must not break the common case.
        let mut query_params = std::collections::HashMap::new();
        query_params.insert("page".to_string(), "2".to_string());
        let req2 = Request {
            url: format!("http://{}/start", addr),
            method: Method::GET,
            follow_redirects: true,
            query_params,
            ..Default::default()
        };
        let resp2 = client.execute(&req2, None).await.unwrap();
        assert_eq!(
            resp2.status_code,
            200,
            "hop 0 with populated query_params should be served; got: {}",
            String::from_utf8_lossy(&resp2.body)
        );
        assert_eq!(
            String::from_utf8_lossy(&resp2.body),
            "ok",
            "populated query_params must still be appended once on hop 0; got: {}",
            String::from_utf8_lossy(&resp2.body)
        );

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cross_origin_redirect_strips_signed_authorization() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // P0 corollary: a redirect to a DIFFERENT origin must NOT carry the
        // hop-0 signed Authorization (credential leak). /start on server A
        // → 302 → server B /final; B reports whether it saw the header.
        let listener_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listener_b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr_a = listener_a.local_addr().unwrap();
        let addr_b = listener_b.local_addr().unwrap();

        let server_b = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener_b.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let req = String::from_utf8_lossy(&buf);
                    let saw_auth = req
                        .lines()
                        .any(|l| l.to_ascii_lowercase().starts_with("authorization:"));
                    let body = if saw_auth { "AUTH=YES" } else { "AUTH=NO" };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        let server_a = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener_a.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://{}/final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        addr_b
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        let cfg = HttpConfig::default();
        let client = HttpClient::new(&cfg).unwrap();
        let signer = StaticSigner;
        let req = Request {
            url: format!("http://{}/start", addr_a),
            method: Method::GET,
            follow_redirects: true,
            ..Default::default()
        };
        let resp = client.execute(&req, Some(&signer)).await.unwrap();
        assert_eq!(resp.status_code, 200);
        assert_eq!(
            String::from_utf8_lossy(&resp.body),
            "AUTH=NO",
            "signed Authorization must NOT leak to a different origin"
        );

        server_a.abort();
        server_b.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn discarded_body_still_reuses_pooled_connection() {
        // Regression for TROPEL_TODO_V2: when discardResponseBodies (or
        // responseType: "none") was set, execute() left the body unread and
        // dropped the Response — reqwest then closed the socket, so every
        // request opened a fresh TCP connection (the opposite of pooling).
        // With the drain fix, N sequential requests must ride ONE connection.
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        let connections_srv = connections.clone();

        // Keep-alive server that counts every accepted TCP connection and
        // serves a read-loop (like the subtimings test) so pooled requests
        // can reuse the same socket.
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                connections_srv.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = sock.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        sock.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok",
                        )
                        .await
                        .unwrap();
                    }
                });
            }
        });

        let cfg = HttpConfig::default();
        let client = HttpClient::new(&cfg).unwrap();
        let req = Request {
            url: format!("http://{}/", addr),
            method: Method::GET,
            response_type: tropel_sdk::types::ResponseType::None,
            ..Default::default()
        };

        // Three requests with discarded bodies. Each must return 200 with an
        // EMPTY body (the drain discards the bytes) — and because the body is
        // fully drained, reqwest returns the connection to the pool.
        for i in 0..3 {
            let resp = client.execute(&req, None).await.unwrap();
            assert_eq!(resp.status_code, 200, "request {} failed", i);
            assert!(resp.body.is_empty(), "discarded body must be empty");
            // The body is drained, not skipped: size/data_received still count
            // the wire bytes (Content-Length is 2 here) even though the body
            // is empty.
            assert_eq!(resp.size, 2, "drained bytes must still feed size");
        }

        // Give the server a moment to register any extra connects, then
        // assert the pool was reused: exactly ONE TCP connection for 3
        // requests. (Before the fix this was 3 — one reconnect per request.)
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        server.abort();
        assert_eq!(
            connections.load(Ordering::SeqCst),
            1,
            "discarded bodies must not tear down the pooled connection"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bodies_sent_for_all_methods_including_delete_options_trace_custom() {
        // Regression (backlog line 140): the request builder attached the
        // body ONLY for POST/PUT/PATCH — DELETE/OPTIONS/TRACE and custom
        // methods (PURGE, …) silently dropped it. The builder is now
        // method-agnostic; every method must deliver its body. (Postman's
        // GET/HEAD pruning lives in the collection parser, so the transport
        // must not re-introduce it here.)
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let raw = String::from_utf8_lossy(&buf);
                    let method = raw
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().next())
                        .unwrap_or("")
                        .to_string();
                    let saw_body = raw.contains("hello-from-140");
                    let body = format!("{}:{}", method, if saw_body { "BODY" } else { "NONE" });
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        let cfg = HttpConfig::default();
        let client = HttpClient::new(&cfg).unwrap();

        for method in [
            Method::DELETE,
            Method::OPTIONS,
            Method::TRACE,
            Method::Custom("PURGE".to_string()),
        ] {
            let req = Request {
                url: format!("http://{}/x", addr),
                method: method.clone(),
                body: Some(Body::Raw("hello-from-140".to_string())),
                ..Default::default()
            };
            let resp = client.execute(&req, None).await.unwrap();
            let echo = String::from_utf8_lossy(&resp.body).to_string();
            assert!(
                echo.ends_with(":BODY"),
                "{} must deliver its body, server saw: {}",
                format_args!("{:?}", method),
                echo
            );
        }

        server.abort();
    }

    /// TR-202 twin guard: the Digest challenge-response retry must still work
    /// when a request carries a body. The timed body wrapper is a streaming
    /// body (`reqwest::Body::wrap`), and `Request::try_clone` returns `None`
    /// for streaming bodies — so a signed request must keep the plain
    /// cloneable body, or the 401 retry silently stops happening (the engine
    /// would report the unauthenticated 401 as the request's result). This
    /// test runs a full challenge-response round trip WITH a body and asserts
    /// the retry fires (status 200, not 401).
    #[tokio::test(flavor = "current_thread")]
    async fn digest_retry_with_body_still_retries_when_timed_body_gated_off() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Server: first request gets `401 WWW-Authenticate: Digest ...`, second
        // (retried) request gets 200. The 401 body carries the challenge realm;
        // the retry must carry the request body too.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let raw = String::from_utf8_lossy(&buf[..n]);
                let has_auth = raw.to_ascii_lowercase().contains("authorization:");
                let has_body = raw.contains("digest-body");
                let body = format!("attempt:auth={has_auth},body={has_body}");
                let resp = if has_auth {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                } else {
                    "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"t\", nonce=\"n\", qop=\"auth\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                };
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let signer = tropel_auth::signers::DigestAuth::new("user", "pass");
        let cfg = HttpConfig::default();
        let client = HttpClient::new(&cfg).unwrap();
        let req = Request {
            url: format!("http://{}/x", addr),
            method: Method::POST,
            body: Some(Body::Raw("digest-body".to_string())),
            ..Default::default()
        };

        let resp = client.execute(&req, Some(&signer)).await.unwrap();
        server.abort();

        assert_eq!(
            resp.status_code, 200,
            "Digest retry must fire and succeed (timed-body gating must not \
             disable the 401 challenge-response retry)"
        );
        let echo = String::from_utf8_lossy(&resp.body).to_string();
        assert!(
            echo.contains("body=true"),
            "retried request must carry the request body, server saw: {echo}"
        );
    }

    /// TR-230: a user-supplied `Host` key becomes `req.Host` (k6 parity) —
    /// carried in `request.host`, applied on the wire as the Host header, and
    /// skipped in the plain-header loop (so it never doubles up). A stray
    /// Host header entry from another path is also skipped.
    #[tokio::test(flavor = "current_thread")]
    async fn user_supplied_host_header_overrides_url_host() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let raw = String::from_utf8_lossy(&buf[..n]);
                    // Echo the Host line as the response body.
                    let host_line = raw
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("host:"))
                        .map(|l| l.to_string())
                        .unwrap_or_else(|| "no-host-line".to_string());
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        host_line.len(),
                        host_line
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        let cfg = HttpConfig::default();
        let client = HttpClient::new(&cfg).unwrap();
        let req = Request {
            url: format!("http://{}/x", addr),
            method: Method::GET,
            headers: vec![("Host".to_string(), "should-be-skipped".to_string())],
            host: Some("custom.example.com".to_string()),
            ..Default::default()
        };
        let resp = client.execute(&req, None).await.unwrap();
        server.abort();

        assert_eq!(resp.status_code, 200);
        let body = String::from_utf8_lossy(&resp.body).to_string();
        assert!(
            body.contains("custom.example.com") && !body.contains("should-be-skipped"),
            "request.host must be the wire Host (stray header skipped), server saw: {body}"
        );
    }
}

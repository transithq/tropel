//! Proxy configuration and bypass matching  [ask 17, KP-403]
//!
//! There was NO proxy surface at any layer: `build_client` never called
//! `reqwest::Proxy::*` or `no_proxy`, and `HttpConfig` carried no proxy field.
//! KnockPort's relay already parses a `settings.proxy` block off its
//! `POST /proxy` wire and REFUSES it, naming this ask; every `proxy.*`
//! capability is declared false for the same reason.
//!
//! The bypass matcher is the part worth reading. Its rules are not "whatever
//! `NO_PROXY` happens to do" — they are stated in the ask, and each one exists
//! because the fuzzy alternative silently sends traffic somewhere the user did
//! not intend:
//!
//!   * an exact host matches that host,
//!   * `*.suffix` matches SUBDOMAINS ONLY — `*.example.com` covers
//!     `a.example.com` and NOT `example.com`, because a rule written for
//!     internal subdomains should not silently start covering the apex,
//!   * a bare `*` matches everything,
//!   * an IP matches that address; a CIDR matches its range,
//!   * a wildcard ANYWHERE ELSE is a config error, not a fuzzy match.
//!
//! That last rule is the one most implementations get wrong. `NO_PROXY=exa*le.com`
//! is a typo, and treating it as a glob means traffic the user believed was
//! bypassed goes through the proxy — or worse, traffic they believed was
//! proxied goes direct. Refused by name instead.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

/// How a proxy is chosen for a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMode {
    /// No proxy. The default, and what every existing config gets.
    #[default]
    Off,
    /// One explicitly configured proxy.
    Fixed,
    /// The environment's proxy variables (`HTTPS_PROXY`, `HTTP_PROXY`,
    /// `NO_PROXY`).
    System,
    /// A PAC script, evaluated per URL.
    Pac,
}

/// Proxy configuration.
///
/// One struct for all four modes rather than an enum with per-mode payloads:
/// it crosses a JSON wire (KnockPort's `settings.proxy`) where an
/// externally-tagged enum would change shape per mode, and a UI that lets the
/// user switch mode must not lose the host they typed for `fixed` when they
/// look at `system`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProxyConfig {
    pub mode: ProxyMode,
    /// `http`, `https`, `socks5` — the proxy's own scheme, not the target's.
    pub protocol: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Hosts that must NOT go through the proxy. See the module docs.
    pub bypass: Vec<String>,
    /// `pac` mode: where to fetch the script, or the script itself.
    #[serde(alias = "pacUrl")]
    pub pac_url: Option<String>,
    #[serde(alias = "pacScript")]
    pub pac_script: Option<String>,
}

impl ProxyConfig {
    /// The proxy URL for `fixed` mode, without credentials.
    ///
    /// Credentials go through `Proxy::basic_auth`, never the URL: a proxy URL
    /// with a password in it is logged by everything that logs a URL, and
    /// reqwest would also have to re-encode it.
    pub fn fixed_url(&self) -> Option<String> {
        let host = self.host.as_deref()?.trim();
        if host.is_empty() {
            return None;
        }
        let scheme = self.protocol.as_deref().unwrap_or("http").trim();
        let scheme = if scheme.is_empty() { "http" } else { scheme };
        Some(match self.port {
            Some(port) => format!("{scheme}://{host}:{port}"),
            // No port: let the proxy's own scheme default apply rather than
            // inventing 8080.
            None => format!("{scheme}://{host}"),
        })
    }
}

/// A parsed bypass entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BypassRule {
    /// `*` — everything bypasses.
    All,
    /// An exact hostname.
    Host(String),
    /// `*.suffix` — subdomains of `suffix`, NOT `suffix` itself.
    Subdomains(String),
    Ip(IpAddr),
    /// A CIDR range, kept as (network, prefix length).
    Cidr(IpAddr, u8),
}

/// Parse one bypass entry.
///
/// `Err` names the entry and why. A malformed rule is a config error rather
/// than an entry that quietly matches nothing — an ignored bypass sends
/// traffic through a proxy the user told it to skip.
pub fn parse_bypass(entry: &str) -> Result<BypassRule, String> {
    let raw = entry.trim();
    if raw.is_empty() {
        return Err("an empty bypass entry matches nothing — remove it".to_string());
    }
    if raw == "*" {
        return Ok(BypassRule::All);
    }
    if let Some(suffix) = raw.strip_prefix("*.") {
        if suffix.is_empty() || suffix.contains('*') {
            return Err(format!(
                "bypass entry {raw:?}: \"*.\" must be followed by a plain suffix"
            ));
        }
        return Ok(BypassRule::Subdomains(suffix.to_ascii_lowercase()));
    }
    if raw.contains('*') {
        // THE rule most implementations get wrong. `exa*le.com` is a typo,
        // and globbing it means traffic the user believed was bypassed is
        // proxied — or the reverse. Named, not guessed at.
        return Err(format!(
            "bypass entry {raw:?}: a wildcard is only allowed as a leading \"*.\" \
             (subdomains) or a bare \"*\" (everything)"
        ));
    }
    if let Some((net, prefix)) = raw.split_once('/') {
        let addr: IpAddr = net
            .parse()
            .map_err(|_| format!("bypass entry {raw:?}: {net:?} is not an IP address"))?;
        let bits: u8 = prefix
            .parse()
            .map_err(|_| format!("bypass entry {raw:?}: {prefix:?} is not a prefix length"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        if bits > max {
            return Err(format!("bypass entry {raw:?}: /{bits} exceeds /{max}"));
        }
        return Ok(BypassRule::Cidr(addr, bits));
    }
    if let Ok(addr) = raw.parse::<IpAddr>() {
        return Ok(BypassRule::Ip(addr));
    }
    Ok(BypassRule::Host(raw.to_ascii_lowercase()))
}

impl BypassRule {
    /// Does this rule bypass `host`?
    pub fn matches(&self, host: &str) -> bool {
        let host = host.trim().to_ascii_lowercase();
        match self {
            BypassRule::All => true,
            BypassRule::Host(want) => &host == want,
            BypassRule::Subdomains(suffix) => {
                // Subdomains ONLY. The dot is part of the test, so
                // `*.example.com` does not match `notexample.com` either —
                // a suffix check without it would.
                host.len() > suffix.len() + 1
                    && host.ends_with(suffix.as_str())
                    && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
            }
            BypassRule::Ip(want) => host.parse::<IpAddr>().map(|a| a == *want).unwrap_or(false),
            BypassRule::Cidr(net, bits) => host
                .parse::<IpAddr>()
                .map(|addr| in_cidr(addr, *net, *bits))
                .unwrap_or(false),
        }
    }
}

/// Is `addr` inside `net/bits`?
///
/// Compares the leading `bits` of the address bytes. Written out rather than
/// pulled from `ipnet` because the whole operation is eight lines and the
/// crate is not currently a dependency of this one — a new dependency for a
/// prefix comparison is not a trade worth making.
fn in_cidr(addr: IpAddr, net: IpAddr, bits: u8) -> bool {
    match (addr, net) {
        (IpAddr::V4(a), IpAddr::V4(n)) => prefix_eq(&a.octets(), &n.octets(), bits),
        (IpAddr::V6(a), IpAddr::V6(n)) => prefix_eq(&a.octets(), &n.octets(), bits),
        // A v4 address is never inside a v6 range or the reverse. Returning
        // false rather than mapping v4-into-v6: a `::ffff:10.0.0.0/104` rule
        // meaning "the v4 10/8" is a thing nobody writes deliberately, and
        // guessing would make a bypass fire for an address family the user
        // did not name.
        _ => false,
    }
}

fn prefix_eq(a: &[u8], b: &[u8], bits: u8) -> bool {
    let full = (bits / 8) as usize;
    if a[..full] != b[..full] {
        return false;
    }
    let rest = bits % 8;
    if rest == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - rest);
    a[full] & mask == b[full] & mask
}

/// Parse every bypass entry, or report ALL the bad ones at once.
///
/// All of them, not the first: a user fixing a proxy config wants the whole
/// list of typos, and reporting one per run turns a three-typo config into
/// three failed runs.
pub fn parse_bypass_list(entries: &[String]) -> Result<Vec<BypassRule>, String> {
    let mut rules = Vec::new();
    let mut errors = Vec::new();
    for entry in entries {
        match parse_bypass(entry) {
            Ok(rule) => rules.push(rule),
            Err(e) => errors.push(e),
        }
    }
    if errors.is_empty() {
        Ok(rules)
    } else {
        Err(errors.join("; "))
    }
}

/// Should `host` skip the proxy?
pub fn bypasses(rules: &[BypassRule], host: &str) -> bool {
    rules.iter().any(|r| r.matches(host))
}

/// The environment's proxy settings, for `system` mode.
///
/// Read here rather than left to reqwest's own env handling because the mode
/// has to be OBSERVABLE: `system` must be able to report what it found, and a
/// config that says `system` on a machine with no proxy variables should be
/// distinguishable from one that found a proxy. reqwest's built-in handling is
/// silent either way.
///
/// `HTTPS_PROXY` before `HTTP_PROXY` — the more specific first, matching every
/// other tool's precedence. Lowercase forms are read too, because curl
/// popularised them and half the world sets `https_proxy`.
pub fn system_proxy_url() -> Option<String> {
    for key in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// `NO_PROXY` as bypass entries.
pub fn system_no_proxy() -> Vec<String> {
    for key in ["NO_PROXY", "no_proxy"] {
        if let Ok(value) = std::env::var(key) {
            let entries: Vec<String> = value
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !entries.is_empty() {
                return entries;
            }
        }
    }
    Vec::new()
}

// ── PAC directives and failover  [ask 17] ──────────────────────────────────
//
// The ask carries the spec because this is the differentiator: "Bruno honours
// only the FIRST directive of the first matching return." A PAC script's
// return value is a LIST of candidates separated by `;`, and honouring one of
// them turns a config with a documented fallback into a single point of
// failure.
//
// This half is the parser and the ordering — pure, and where the bug is.
// Evaluating `FindProxyForURL` needs a JS engine and its built-ins
// (`isInNet`, `dnsDomainIs`, `shExpMatch`, …); that is the other half, and it
// wants its own crate rather than a JS engine inside the HTTP client.

/// One PAC directive — a single candidate to try.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PacDirective {
    /// `DIRECT` — no proxy for this URL.
    Direct,
    /// `PROXY host:port` — an HTTP proxy.
    Proxy(String),
    /// `SOCKS host:port` (also `SOCKS4`/`SOCKS5`).
    Socks(String),
}

impl PacDirective {
    /// The proxy URL to hand reqwest, or `None` for `DIRECT`.
    pub fn proxy_url(&self) -> Option<String> {
        match self {
            PacDirective::Direct => None,
            PacDirective::Proxy(target) => Some(format!("http://{target}")),
            // `socks5://`, not `socks4://`: reqwest's socks support is
            // SOCKS5, and a PAC `SOCKS` directive predates the distinction.
            // Naming 5 is honest about what would actually be attempted.
            PacDirective::Socks(target) => Some(format!("socks5://{target}")),
        }
    }
}

/// Parse a PAC return value into candidates, IN ORDER.
///
/// `"PROXY a:8080; PROXY b:8080; DIRECT"` is three candidates, tried in that
/// order. Returning only the first is the Bruno bug this exists to avoid.
///
/// Unparseable directives are SKIPPED rather than failing the whole result: a
/// PAC script is often generated, and one malformed entry in a list of four
/// should not take the other three down with it. An entirely unparseable
/// result yields an empty list, which the caller must treat as a failure
/// rather than as "go direct" — going direct on a PAC script we could not
/// read is the silent misroute the whole surface exists to prevent.
pub fn parse_pac_result(result: &str) -> Vec<PacDirective> {
    result
        .split(';')
        .filter_map(|candidate| parse_pac_directive(candidate.trim()))
        .collect()
}

fn parse_pac_directive(directive: &str) -> Option<PacDirective> {
    if directive.is_empty() {
        return None;
    }
    // Split on ANY whitespace, not just one space: `PROXY   host:80` and
    // `PROXY\thost:80` both occur in generated scripts.
    let mut parts = directive.split_whitespace();
    let keyword = parts.next()?.to_ascii_uppercase();
    let target = parts.next();
    match (keyword.as_str(), target) {
        ("DIRECT", _) => Some(PacDirective::Direct),
        // `PROXY` with no target is malformed — skipped rather than turned
        // into a DIRECT, which would send traffic somewhere the script did
        // not say.
        ("PROXY", Some(t)) | ("HTTP", Some(t)) => Some(PacDirective::Proxy(t.to_string())),
        ("HTTPS", Some(t)) => Some(PacDirective::Proxy(t.to_string())),
        ("SOCKS", Some(t)) | ("SOCKS4", Some(t)) | ("SOCKS5", Some(t)) => {
            Some(PacDirective::Socks(t.to_string()))
        }
        _ => None,
    }
}

/// A per-host PAC decision, cached for a bounded time.
///
/// Bounded because a PAC script's answer can change (a laptop moves networks)
/// and unbounded caching would pin the old answer for the process's life. The
/// ask names k6's `dns.ttl` discipline as the model.
#[derive(Debug, Clone)]
pub struct PacDecision {
    pub candidates: Vec<PacDirective>,
    /// Which candidate is currently believed good. Advanced on failure.
    pub current: usize,
    pub decided_at: std::time::Instant,
}

impl PacDecision {
    pub fn new(candidates: Vec<PacDirective>) -> Self {
        Self { candidates, current: 0, decided_at: std::time::Instant::now() }
    }

    /// The candidate to try now, or `None` when every one has failed.
    pub fn candidate(&self) -> Option<&PacDirective> {
        self.candidates.get(self.current)
    }

    /// This candidate failed to CONNECT — advance to the next.
    ///
    /// Only connect/TLS/unsatisfiable-407 failures advance. An HTTP error
    /// RESPONSE (a 500 from the target) means the proxy worked, so advancing
    /// on it would try every candidate for a fault none of them can fix.
    pub fn advance(&mut self) -> Option<&PacDirective> {
        self.current += 1;
        self.candidate()
    }

    pub fn exhausted(&self) -> bool {
        self.current >= self.candidates.len()
    }

    pub fn is_stale(&self, ttl: std::time::Duration) -> bool {
        self.decided_at.elapsed() >= ttl
    }
}

/// Default PAC decision TTL.
pub const DEFAULT_PAC_TTL: std::time::Duration = std::time::Duration::from_secs(300);

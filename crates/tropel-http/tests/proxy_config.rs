//! Ask 17 · proxy configuration and bypass matching.
//!
//! There was NO proxy surface at any layer. KnockPort's relay already parses a
//! `settings.proxy` block off its `POST /proxy` wire and REFUSES it, naming
//! this ask; every `proxy.*` capability is declared false for the same reason.
//!
//! Most of these tests are about the BYPASS matcher, because that is where a
//! mistake misroutes traffic silently — either sending it through a proxy the
//! user excluded, or sending it direct when they expected the proxy. Both are
//! worse than a loud failure, so the rules are exact and a malformed rule is
//! an error rather than a fuzzy match.

use tropel_http::{
    BypassRule, HttpClient, HttpConfig, ProxyConfig, ProxyMode, TlsConfig, bypasses, parse_bypass,
    parse_bypass_list,
};

fn rules(entries: &[&str]) -> Vec<BypassRule> {
    parse_bypass_list(&entries.iter().map(|e| (*e).to_string()).collect::<Vec<_>>())
        .expect("entries parse")
}

// ── The rule most implementations get wrong ────────────────────────────────

#[test]
fn a_subdomain_rule_does_not_match_the_apex() {
    // `*.example.com` covers `a.example.com` and NOT `example.com`. A rule
    // written for internal subdomains should not silently start covering the
    // apex — that is a different host, often on a different network.
    let r = rules(&["*.example.com"]);
    assert!(bypasses(&r, "a.example.com"));
    assert!(bypasses(&r, "deep.nested.example.com"));
    assert!(!bypasses(&r, "example.com"), "the apex is NOT a subdomain");
}

#[test]
fn a_subdomain_rule_does_not_match_a_lookalike_suffix() {
    // The dot is part of the test. A bare `ends_with` would match
    // `notexample.com` against `*.example.com` — a completely unrelated
    // domain someone else owns.
    let r = rules(&["*.example.com"]);
    assert!(!bypasses(&r, "notexample.com"));
    assert!(!bypasses(&r, "evilexample.com"));
}

#[test]
fn a_wildcard_anywhere_else_is_a_config_error() {
    // THE rule. `exa*le.com` is a typo, and globbing it means traffic the
    // user believed was bypassed is proxied — or the reverse. Named, not
    // guessed at.
    for bad in ["exa*le.com", "example.*", "*example.com", "a.*.example.com"] {
        let err = parse_bypass(bad).expect_err(&format!("{bad} must be refused"));
        assert!(err.contains(bad), "names the entry: {err}");
        assert!(err.contains("wildcard"), "says why: {err}");
    }
}

#[test]
fn a_bare_star_matches_everything() {
    let r = rules(&["*"]);
    assert!(bypasses(&r, "example.com"));
    assert!(bypasses(&r, "10.0.0.1"));
    assert!(bypasses(&r, "anything.at.all"));
}

#[test]
fn an_exact_host_matches_only_itself() {
    let r = rules(&["api.internal"]);
    assert!(bypasses(&r, "api.internal"));
    assert!(!bypasses(&r, "other.internal"));
    // Not its own subdomains either — that is what `*.` is for.
    assert!(!bypasses(&r, "v2.api.internal"));
}

#[test]
fn host_matching_is_case_insensitive() {
    // DNS is. A rule that missed `API.Internal` would be a bypass that
    // depends on how someone typed a URL.
    let r = rules(&["api.internal", "*.example.com"]);
    assert!(bypasses(&r, "API.Internal"));
    assert!(bypasses(&r, "A.Example.COM"));
}

// ── IPs and CIDR ───────────────────────────────────────────────────────────

#[test]
fn an_ip_entry_matches_that_address() {
    let r = rules(&["10.0.0.1", "::1"]);
    assert!(bypasses(&r, "10.0.0.1"));
    assert!(bypasses(&r, "::1"));
    assert!(!bypasses(&r, "10.0.0.2"));
}

#[test]
fn a_cidr_entry_matches_its_range() {
    let r = rules(&["10.0.0.0/8", "192.168.1.0/24"]);
    assert!(bypasses(&r, "10.255.255.255"));
    assert!(bypasses(&r, "10.0.0.1"));
    assert!(bypasses(&r, "192.168.1.7"));
    assert!(!bypasses(&r, "192.168.2.7"), "/24 stops at the third octet");
    assert!(!bypasses(&r, "11.0.0.1"));
}

#[test]
fn a_cidr_with_a_partial_byte_prefix_masks_correctly() {
    // /12 is the case a byte-aligned-only implementation gets wrong:
    // 172.16.0.0/12 covers 172.16 through 172.31 and must stop at 172.32.
    let r = rules(&["172.16.0.0/12"]);
    assert!(bypasses(&r, "172.16.0.1"));
    assert!(bypasses(&r, "172.31.255.254"));
    assert!(!bypasses(&r, "172.32.0.1"), "/12 stops at 172.31");
    assert!(!bypasses(&r, "172.15.255.255"));
}

#[test]
fn a_v4_address_is_not_inside_a_v6_range_or_the_reverse() {
    // Guessing would make a bypass fire for an address family the user did
    // not name.
    let v6 = rules(&["fd00::/8"]);
    assert!(!bypasses(&v6, "10.0.0.1"));
    let v4 = rules(&["10.0.0.0/8"]);
    assert!(!bypasses(&v4, "fd00::1"));
}

#[test]
fn a_nonsense_cidr_is_refused_with_the_reason() {
    for (bad, why) in [
        ("10.0.0.0/33", "/33"),
        ("::1/129", "/129"),
        ("notanip/8", "not an IP"),
        ("10.0.0.0/abc", "prefix length"),
    ] {
        let err = parse_bypass(bad).expect_err(&format!("{bad} must be refused"));
        assert!(err.contains(why), "for {bad}, got: {err}");
    }
}

#[test]
fn an_empty_entry_is_refused_rather_than_matching_nothing_quietly() {
    // A trailing comma in `NO_PROXY` produces one. Silently keeping a rule
    // that matches nothing is how a bypass list looks longer than it is.
    let err = parse_bypass("   ").expect_err("must refuse");
    assert!(err.contains("empty"), "got: {err}");
}

#[test]
fn every_bad_entry_is_reported_at_once() {
    // A user fixing a proxy config wants the whole list of typos. Reporting
    // one per run turns a three-typo config into three failed runs.
    let err = parse_bypass_list(&[
        "good.example".to_string(),
        "ba*d.example".to_string(),
        "10.0.0.0/99".to_string(),
    ])
    .expect_err("must refuse");
    assert!(err.contains("ba*d.example"), "got: {err}");
    assert!(err.contains("/99"), "reports BOTH: {err}");
}

// ── The client build ───────────────────────────────────────────────────────

/// Did the client BUILD? Returns unit rather than the client because no test
/// here uses one, and `HttpClient` has no `Debug` for `expect_err` to format.
fn build(proxy: ProxyConfig) -> Result<(), String> {
    let config = HttpConfig { proxy, ..HttpConfig::default() };
    HttpClient::with_tls(&config, &TlsConfig::default())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[test]
fn the_default_is_off_and_every_existing_config_is_unchanged() {
    assert_eq!(ProxyConfig::default().mode, ProxyMode::Off);
    assert!(build(ProxyConfig::default()).is_ok());
}

#[test]
fn fixed_mode_builds_with_a_host() {
    let client = build(ProxyConfig {
        mode: ProxyMode::Fixed,
        host: Some("proxy.internal".into()),
        port: Some(3128),
        ..Default::default()
    });
    assert!(client.is_ok(), "{:?}", client.err());
}

#[test]
fn fixed_mode_with_no_host_is_refused_by_name() {
    // The alternative is a client that says it proxies and doesn't.
    let err = build(ProxyConfig { mode: ProxyMode::Fixed, ..Default::default() })
        .expect_err("must refuse");
    assert!(err.contains("no host is set"), "got: {err}");
}

#[test]
fn fixed_mode_carries_credentials_without_putting_them_in_the_url() {
    // A proxy URL with a password in it is logged by everything that logs a
    // URL, so credentials go through `basic_auth`.
    let cfg = ProxyConfig {
        mode: ProxyMode::Fixed,
        host: Some("proxy.internal".into()),
        port: Some(3128),
        username: Some("u".into()),
        password: Some("p".into()),
        ..Default::default()
    };
    let url = cfg.fixed_url().expect("a url");
    assert_eq!(url, "http://proxy.internal:3128");
    assert!(!url.contains('u') || !url.contains("u:p"), "no credentials in the URL: {url}");
    assert!(build(cfg).is_ok());
}

#[test]
fn a_fixed_url_defaults_to_http_and_omits_an_absent_port() {
    // No port: let the proxy scheme's own default apply rather than
    // inventing 8080.
    let cfg = ProxyConfig {
        mode: ProxyMode::Fixed,
        host: Some("proxy.internal".into()),
        ..Default::default()
    };
    assert_eq!(cfg.fixed_url().as_deref(), Some("http://proxy.internal"));
}

#[test]
fn a_bypass_typo_fails_the_build_rather_than_misrouting_later() {
    // The whole reason the list is parsed before the proxy is installed.
    let err = build(ProxyConfig {
        mode: ProxyMode::Fixed,
        host: Some("proxy.internal".into()),
        bypass: vec!["ba*d.example".to_string()],
        ..Default::default()
    })
    .expect_err("must refuse");
    assert!(err.contains("bypass list"), "got: {err}");
    assert!(err.contains("ba*d.example"), "names the entry: {err}");
}

#[test]
fn pac_mode_is_refused_by_name_rather_than_degrading_to_direct() {
    // PAC needs per-URL evaluation with directive failover, which a
    // build-time hook cannot do. Sending traffic direct while the config says
    // `pac` would be exactly the silent misroute this surface prevents.
    let err = build(ProxyConfig {
        mode: ProxyMode::Pac,
        pac_url: Some("http://wpad/wpad.dat".into()),
        ..Default::default()
    })
    .expect_err("must refuse");
    assert!(err.contains("not implemented yet"), "got: {err}");
    assert!(err.contains("ask 17"), "cites the ask: {err}");
    assert!(err.contains("failover"), "says what is missing: {err}");
}

#[test]
fn off_mode_says_no_proxy_explicitly() {
    // reqwest reads `HTTP_PROXY` from the environment on its own, so "off"
    // has to SAY so — otherwise a machine with the variable set would quietly
    // proxy a config that asked for no proxy at all. Asserted through the
    // build succeeding with a hostile environment variable present.
    //
    // SAFETY: single-threaded test, and the variable is removed immediately.
    unsafe { std::env::set_var("HTTP_PROXY", "http://should-not-be-used.invalid:1") };
    let built = build(ProxyConfig::default());
    unsafe { std::env::remove_var("HTTP_PROXY") };
    assert!(built.is_ok(), "{:?}", built.err());
}

// ── The JSON surface KnockPort writes ──────────────────────────────────────

#[test]
fn the_wire_shape_knockport_already_sends_deserializes() {
    // KnockPort's relay carries exactly this block on `POST /proxy` and
    // refuses it today, naming this ask. It has to read without a shape
    // change or the refusal cannot simply be removed.
    let cfg: ProxyConfig = serde_json::from_str(
        r#"{"mode":"fixed","protocol":"http","host":"proxy.internal","port":3128,
             "username":"u","password":"p","bypass":["localhost","*.internal"]}"#,
    )
    .expect("reads");
    assert_eq!(cfg.mode, ProxyMode::Fixed);
    assert_eq!(cfg.port, Some(3128));
    assert_eq!(cfg.bypass.len(), 2);
}

#[test]
fn an_absent_proxy_block_is_off() {
    let cfg: HttpConfig = serde_json::from_str("{}").expect("reads");
    assert_eq!(cfg.proxy.mode, ProxyMode::Off);
}

#[test]
fn pac_fields_read_in_both_spellings() {
    for json in [r#"{"mode":"pac","pac_url":"http://a/x.dat"}"#, r#"{"mode":"pac","pacUrl":"http://a/x.dat"}"#] {
        let cfg: ProxyConfig = serde_json::from_str(json).expect("reads");
        assert_eq!(cfg.pac_url.as_deref(), Some("http://a/x.dat"), "for {json}");
    }
}

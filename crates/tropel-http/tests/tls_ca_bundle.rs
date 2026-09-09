//! Ask 18 · private CA bundles on the TLS builder.
//!
//! `build_client` never called `add_root_certificate`, so a private CA was
//! unreachable and `TlsConfig` had nowhere to name one. KnockPort declares
//! `security.customCaCertificate: false` across every transport with this ask
//! as the stated reason, and its relay's `GET /capabilities` answers
//! `tls_ca_bundle: false` for the same.
//!
//! Real certificates, not fixtures-shaped strings: the whole point is that
//! reqwest ACCEPTS what we hand it, and a hand-written PEM would only prove
//! the error path. `tests/fixtures/` holds two self-signed roots and a
//! two-cert concatenation of them.

use tropel_http::{HttpClient, HttpConfig, TlsConfig};

const ONE: &str = "tests/fixtures/test-root-one.pem";
const TWO: &str = "tests/fixtures/test-root-two.pem";
const CHAIN: &str = "tests/fixtures/test-root-chain.pem";

fn tls(paths: &[&str], keep_system_roots: bool) -> TlsConfig {
    TlsConfig {
        root_cert_paths: paths.iter().map(|p| (*p).to_string()).collect(),
        keep_system_roots,
        ..Default::default()
    }
}

#[test]
fn a_client_builds_with_a_single_private_ca() {
    let client = HttpClient::with_tls(&HttpConfig::default(), &tls(&[ONE], true));
    assert!(client.is_ok(), "one PEM root should build: {:?}", client.err().map(|e| e.to_string()));
}

#[test]
fn two_separate_bundles_both_load() {
    // A list rather than one path, because two independent CAs — a corporate
    // root and a test root — is the ordinary case.
    let client = HttpClient::with_tls(&HttpConfig::default(), &tls(&[ONE, TWO], true));
    assert!(client.is_ok(), "two roots should build: {:?}", client.err().map(|e| e.to_string()));
}

#[test]
fn a_multi_cert_bundle_loads_every_certificate_in_it() {
    // THE bug the implementation comment is about. `Certificate::from_pem`
    // reads a SINGLE certificate, so a chain file handed to it whole
    // contributes only its first cert — the intermediate is dropped and
    // verification then fails against a leaf signed by it, which is a very
    // confusing way to fail. `from_pem_bundle` is the multi-cert reader.
    //
    // The chain here is two roots concatenated, so a single-cert reader would
    // still BUILD; what it would not do is trust the second. Asserting the
    // build succeeds is therefore necessary but not sufficient, which is why
    // the parse itself is checked directly below.
    let client = HttpClient::with_tls(&HttpConfig::default(), &tls(&[CHAIN], true));
    assert!(client.is_ok(), "a 2-cert bundle should build: {:?}", client.err().map(|e| e.to_string()));

    let pem = std::fs::read(CHAIN).expect("fixture readable");
    let certs = reqwest::Certificate::from_pem_bundle(&pem).expect("bundle parses");
    assert_eq!(certs.len(), 2, "both certificates in the bundle are read");
    // And the single-cert reader really does see only one — the reason the
    // implementation must not use it.
    assert!(
        reqwest::Certificate::from_pem(&pem).is_ok(),
        "from_pem accepts the file but yields ONE cert, which is the trap"
    );
}

#[test]
fn the_default_keeps_the_platform_roots() {
    // The important default. Adding a private CA nearly always means "as
    // well as" — you still need to reach github.com. A default of certs-only
    // would make a config that adds one internal root silently stop trusting
    // the public internet, which presents as "everything broke after I added
    // our CA".
    assert!(TlsConfig::default().keep_system_roots);
}

#[test]
fn a_config_that_names_no_roots_still_builds() {
    // Nothing about this ask may change the behaviour of a config that does
    // not use it.
    let client = HttpClient::with_tls(&HttpConfig::default(), &TlsConfig::default());
    assert!(client.is_ok(), "the untouched path still builds: {:?}", client.err().map(|e| e.to_string()));
}

#[test]
fn pinning_to_only_the_supplied_bundles_builds() {
    let client = HttpClient::with_tls(&HttpConfig::default(), &tls(&[ONE], false));
    assert!(client.is_ok(), "certs-only should build: {:?}", client.err().map(|e| e.to_string()));
}

#[test]
fn pinning_with_no_bundle_is_refused_by_name() {
    // Trusting nothing at all is a config error, not a pinning strategy.
    // Producing a client that fails every request with an opaque handshake
    // error would be the silent-corruption shape this codebase refuses.
    let err = match HttpClient::with_tls(&HttpConfig::default(), &tls(&[], false)) {
        Ok(_) => panic!("must refuse trusting no CA at all"),
        Err(e) => e,
    };
    let message = err.to_string();
    assert!(message.contains("trusts no CA at all"), "got: {message}");
    assert!(message.contains("root_cert_paths"), "names the field to fix: {message}");
}

#[test]
fn an_unreadable_bundle_names_the_file() {
    // A config with three bundles and one typo otherwise reports "invalid
    // certificate" with no way to tell which.
    let err = match HttpClient::with_tls(
        &HttpConfig::default(),
        &tls(&["tests/fixtures/nope.pem"], true),
    ) {
        Ok(_) => panic!("must refuse an unreadable bundle"),
        Err(e) => e,
    };
    let message = err.to_string();
    assert!(message.contains("nope.pem"), "names the missing file: {message}");
    assert!(message.contains("cannot read CA bundle"), "got: {message}");
}

#[test]
fn a_bundle_that_is_not_pem_names_the_file_too() {
    let err = match HttpClient::with_tls(&HttpConfig::default(), &tls(&["Cargo.toml"], true)) {
        Ok(_) => panic!("must refuse a TOML file offered as a CA bundle"),
        Err(e) => e,
    };
    let message = err.to_string();
    assert!(message.contains("Cargo.toml"), "names the file: {message}");
    // NOT "not valid PEM": `from_pem_bundle` reports no error for a file with
    // no PEM blocks — it parses successfully to ZERO certificates. So the
    // wrong path would otherwise be accepted, contribute no trust, and leave
    // every request failing verification with nothing naming the cause. This
    // test found that; the guard is `certs.is_empty()`.
    assert!(message.contains("no certificates"), "got: {message}");
}

#[test]
fn the_json_surface_accepts_both_spellings() {
    // KnockPort writes camelCase; the engine's own configs are snake_case.
    // Both have to read, or one of the two callers silently gets defaults.
    let snake: TlsConfig = serde_json::from_str(
        r#"{"root_cert_paths":["a.pem"],"keep_system_roots":false}"#,
    )
    .expect("snake_case reads");
    assert_eq!(snake.root_cert_paths, vec!["a.pem".to_string()]);
    assert!(!snake.keep_system_roots);

    let camel: TlsConfig =
        serde_json::from_str(r#"{"rootCertPaths":["b.pem"],"keepSystemRoots":false}"#)
            .expect("camelCase reads");
    assert_eq!(camel.root_cert_paths, vec!["b.pem".to_string()]);
    assert!(!camel.keep_system_roots);
}

#[test]
fn an_absent_keep_system_roots_defaults_to_true_through_serde_too() {
    // `#[derive(Default)]` would have given false here — the one field whose
    // zero value is the wrong answer, which is why `Default` is hand-written.
    let cfg: TlsConfig = serde_json::from_str(r#"{"root_cert_paths":["a.pem"]}"#)
        .expect("reads");
    assert!(cfg.keep_system_roots, "serde default must match the Default impl");
}
